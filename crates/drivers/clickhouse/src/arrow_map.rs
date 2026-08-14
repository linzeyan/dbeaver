//! ClickHouse's types to Arrow's, decided before a single row has moved.
//!
//! The other drivers read a value at a time and build an Arrow array from it.
//! Here the server builds the arrays, so there is no per-value decoder — but
//! that does not make this module smaller, because the server's `ArrowStream`
//! output and the reader at the other end of the FFI disagree about which Arrow
//! types exist, and this is where the disagreement is settled.
//!
//! **The rule.** Every column arrives as one of the twelve Arrow types
//! `apps/macos/Sources/DbClient/ArrowTable.swift` maps a format string for:
//! `Boolean`, `Int16`, `Int32`, `Int64`, `Float32`, `Float64`, `Utf8`,
//! `Binary`, `Decimal128`, `Date32`, `Time64(µs)`, `Timestamp(µs)`. Where a
//! wider Arrow type exists but that reader has no case for it, this picks the
//! narrowest readable type that still holds the value exactly; where none does,
//! it takes ClickHouse's own text rendering. A column the grid draws as `<+s>`
//! in every cell has not been mapped, it has been abandoned.
//!
//! **Why the conversion is a projection and not an Arrow pass.** ClickHouse
//! 24.10 does not merely flatten `UUID`, `JSON` and `Interval*` on the way out —
//! it refuses them, and the refusal takes the whole statement with it:
//!
//! ```text
//! Code: 50. DB::Exception: The type 'UUID' of a column 'uid' is not supported
//! for conversion into Arrow data format. (UNKNOWN_TYPE)
//! ```
//!
//! There is no `output_format_arrow_unsupported_types_as_binary` on that server
//! to soften it — the setting does not exist. So `SELECT *` from any table with
//! a UUID column cannot be answered by fixing up the batch afterwards; there is
//! no batch. The statement has to be asked differently, which means a SELECT
//! list built from the declared types, which means the whole conversion may as
//! well happen there: `toString(uid)` costs the server a rendering it is very
//! good at, and it is the same rendering the user would see in `clickhouse-client`.
//!
//! That the schema is then known exactly before the first row is the same
//! property `DESCRIBE` was going to be needed for anyway.

use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use std::sync::Arc;

/// The Arrow type a projected column arrives in, and how to ask for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mapping {
    /// `None` where the column already arrives in a shape both ends can read,
    /// so that a statement over ordinary columns is sent as the caller wrote it.
    pub cast: Option<Cast>,
    pub arrow: DataType,
    pub nullable: bool,
}

/// The SQL that turns one column into something both ends can carry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cast {
    /// ClickHouse's own rendering. The universal answer, and the only one for
    /// anything this driver has not been taught — an unknown type is still a
    /// column somebody wants to look at.
    Text,
    /// A signed Arrow integer wide enough to hold an unsigned or narrow one.
    /// Lossless: `UInt32` in an `Int64` cannot overflow.
    Widen(&'static str),
    /// `UInt64` has no signed Arrow home, and `Decimal128(20, 0)` is the
    /// narrowest exact one — 20 digits is exactly its range. Rendering it as
    /// text instead would right-align it nowhere and sort it as a string.
    Unsigned64 { nullable: bool },
    /// `Date` arrives as the `UInt16` day count it is stored as, which is a
    /// number in the grid rather than a date.
    Day,
    /// Microseconds, in the zone the declared type names. `DateTime` arrives as
    /// a bare `UInt32`, and `DateTime64` in whatever unit it was declared with,
    /// of which only microseconds are readable at the other end.
    Micros(String),
    /// Text, with invalid byte sequences replaced. Only used by the retry, and
    /// never by a first attempt — see `plan`.
    ValidText,
}

impl Cast {
    /// The expression that reads `column`, which must already be quoted.
    fn apply(&self, column: &str) -> String {
        match self {
            Cast::Text => format!("toString({column})"),
            Cast::ValidText => format!("toValidUTF8(toString({column}))"),
            Cast::Widen(f) => format!("{f}({column})"),
            Cast::Unsigned64 { nullable: false } => {
                format!("CAST({column} AS Decimal(20, 0))")
            }
            // `CAST(NULL AS Decimal(20, 0))` is error 349, not NULL, so the
            // target type has to carry the nullability the source had.
            Cast::Unsigned64 { nullable: true } => {
                format!("CAST({column} AS Nullable(Decimal(20, 0)))")
            }
            Cast::Day => format!("toDate32({column})"),
            Cast::Micros(zone) => {
                format!("toDateTime64({column}, 6, '{}')", escape_literal(zone))
            }
        }
    }
}

/// What one statement's result will look like, and how to ask for it.
pub(crate) struct Plan {
    pub schema: SchemaRef,
    /// The SELECT list to wrap the caller's statement in, or `None` where it can
    /// be sent untouched.
    pub select_list: Option<String>,
}

/// Plans the projection for a statement whose columns `DESCRIBE` has named.
///
/// `sanitize` is the second attempt at a statement whose text columns turned out
/// not to be text. ClickHouse's `String` is arbitrary bytes and the server does
/// not check that it is UTF-8, so a single bad row fails the Arrow decoder and
/// takes the whole result with it. The retry asks for the same columns through
/// `toValidUTF8`, which cannot fail, and — this is why the retry is invisible —
/// arrives as the same Arrow `Utf8` the first attempt promised, so the schema
/// handed out before the first batch is still the truth.
pub(crate) fn plan(columns: &[(String, String)], server_tz: &str, sanitize: bool) -> Plan {
    let mut fields = Vec::with_capacity(columns.len());
    let mut projected = Vec::with_capacity(columns.len());
    let mut needs_wrapping = false;

    for (name, declared) in columns {
        let mut mapping = map(declared, server_tz);
        if sanitize && mapping.arrow == DataType::Utf8 {
            mapping.cast = Some(Cast::ValidText);
        }
        fields.push(Field::new(name, mapping.arrow, mapping.nullable));

        let quoted = quote_identifier(name);
        match &mapping.cast {
            None => projected.push(quoted),
            Some(cast) => {
                needs_wrapping = true;
                projected.push(format!(
                    "{} AS {}",
                    cast.apply(&quoted),
                    quote_identifier(name)
                ));
            }
        }
    }

    Plan {
        schema: Arc::new(Schema::new(fields)),
        select_list: needs_wrapping.then(|| projected.join(", ")),
    }
}

/// One declared ClickHouse type, as `DESCRIBE` spells it.
///
/// `server_tz` is what a `DateTime` with no zone of its own means. It is read
/// from the server rather than assumed to be UTC, because the Arrow field
/// carries the zone name and a driver that guessed it would label every
/// timestamp on a `Asia/Taipei` server wrongly.
pub(crate) fn map(declared: &str, server_tz: &str) -> Mapping {
    let (core, nullable) = unwrap(declared.trim());
    let (head, args) = split(core);

    let (cast, arrow) = match head {
        "Bool" => (None, DataType::Boolean),
        "Int16" => (None, DataType::Int16),
        "Int32" => (None, DataType::Int32),
        "Int64" => (None, DataType::Int64),
        "Float32" => (None, DataType::Float32),
        "Float64" => (None, DataType::Float64),
        "String" => (None, DataType::Utf8),

        // Arrow has `Int8`, `UInt8`, `UInt16` and `UInt32`; the reader at the
        // other end has none of them, and each fits a signed type it does have.
        "Int8" | "UInt8" => (Some(Cast::Widen("toInt16")), DataType::Int16),
        "UInt16" => (Some(Cast::Widen("toInt32")), DataType::Int32),
        "UInt32" => (Some(Cast::Widen("toInt64")), DataType::Int64),
        "UInt64" => (
            Some(Cast::Unsigned64 { nullable }),
            DataType::Decimal128(20, 0),
        ),

        // `FixedString(n)` arrives as `FixedSizeBinary(n)` unless asked
        // otherwise, and the reader has no case for that. Asked otherwise it
        // becomes an ordinary string, padding NULs and all — the setting that
        // makes the difference is pinned in `lib.rs`, and without it this row is
        // wrong rather than merely narrow. Bytes that are not text are the same
        // problem `String` has and get the same answer.
        "FixedString" => (None, DataType::Utf8),

        // `DESCRIBE` normalizes `Decimal32(4)` to `Decimal(9, 4)`, so the width
        // is always spelled out and never has to be inferred from the alias.
        // Past 38 digits it is `Decimal256`, which the reader would parse as a
        // `Decimal128` and read half of — visibly wrong digits rather than a
        // missing column, which is worse.
        "Decimal" => match decimal_width(args) {
            Some((precision, scale)) if precision <= 38 => {
                (None, DataType::Decimal128(precision, scale))
            }
            _ => (Some(Cast::Text), DataType::Utf8),
        },

        "Date32" => (None, DataType::Date32),
        "Date" => (Some(Cast::Day), DataType::Date32),

        "DateTime" => {
            let zone = quoted_arg(args).unwrap_or(server_tz).to_string();
            (
                Some(Cast::Micros(zone.clone())),
                DataType::Timestamp(TimeUnit::Microsecond, Some(zone.into())),
            )
        }
        // Past microseconds there is nowhere to put the extra digits: the reader
        // has one timestamp case and it is microseconds. Truncating would drop
        // three digits of a value somebody chose nanosecond precision for, so
        // the text keeps all nine instead.
        "DateTime64" => {
            let precision = args
                .and_then(|a| {
                    top_level(a)
                        .first()
                        .and_then(|p| p.trim().parse::<u8>().ok())
                })
                .unwrap_or(3);
            if precision <= 6 {
                let zone = args
                    .and_then(|a| top_level(a).get(1).and_then(|z| quoted(z.trim())))
                    .unwrap_or(server_tz)
                    .to_string();
                (
                    Some(Cast::Micros(zone.clone())),
                    DataType::Timestamp(TimeUnit::Microsecond, Some(zone.into())),
                )
            } else {
                (Some(Cast::Text), DataType::Utf8)
            }
        }

        // Everything else, and deliberately one arm rather than a list.
        //
        // `Enum8` and `Enum16` arrive as the ordinals with the labels gone;
        // `UUID`, `JSON` and `Interval*` are refused outright; `IPv4` is a
        // `UInt32`; `IPv6`, `Int128` and `Int256` are fixed-size binary the
        // reader cannot open; `Array`, `Tuple`, `Map` and `Nested` become Arrow
        // nested types it reads as `<+l>`, `<+s>` and `<+m>` in every cell.
        // ClickHouse renders all of them itself, correctly and in the form the
        // user would type — `draft`, `255.255.255.255`, `[1,-1,2147483647]`,
        // `{'a':'1'}` — and a type this driver has never heard of gets the same
        // treatment rather than an apology.
        _ => (Some(Cast::Text), DataType::Utf8),
    };

    Mapping {
        cast,
        arrow,
        nullable,
    }
}

/// Whether a declared type admits NULL.
///
/// `system.columns` has no `is_nullable` — ClickHouse spells nullability inside
/// the type, and `LowCardinality` may wrap it, so
/// `LowCardinality(Nullable(String))` is a nullable column that a prefix check
/// would report as NOT NULL. The catalog and the type mapping ask the same
/// question, so they ask it in the same place.
pub(crate) fn is_nullable(declared: &str) -> bool {
    unwrap(declared.trim()).1
}

/// Strips the wrappers that are not types in their own right.
///
/// `LowCardinality` may sit outside `Nullable` — `LowCardinality(Nullable(String))`
/// is a nullable column — so both are peeled until neither is left, rather than
/// checking for one prefix and calling it done.
fn unwrap(declared: &str) -> (&str, bool) {
    let mut core = declared;
    let mut nullable = false;
    loop {
        let (head, args) = split(core);
        match (head, args) {
            ("Nullable", Some(inner)) => {
                nullable = true;
                core = inner.trim();
            }
            ("LowCardinality", Some(inner)) => core = inner.trim(),
            _ => return (core, nullable),
        }
    }
}

/// A declared type as its constructor and whatever is inside the parentheses.
fn split(declared: &str) -> (&str, Option<&str>) {
    match declared.find('(') {
        Some(open) if declared.ends_with(')') => (
            declared[..open].trim(),
            Some(&declared[open + 1..declared.len() - 1]),
        ),
        _ => (declared.trim(), None),
    }
}

/// The comma-separated arguments of one type, without descending into nested
/// ones.
///
/// Quotes are tracked because a timezone name is an ordinary argument and
/// `DateTime64(9, 'America/Port_of_Spain')` would otherwise split inside it if
/// anybody ever names a zone with a comma; parentheses because
/// `Map(String, Array(Int64))` has two arguments and not three.
fn top_level(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quoted, mut escaped, mut start) = (0usize, false, false, 0usize);
    for (at, c) in args.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted {
            match c {
                '\\' => escaped = true,
                '\'' => quoted = false,
                _ => {}
            }
        } else {
            match c {
                '\'' => quoted = true,
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    out.push(&args[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
        }
    }
    out.push(&args[start..]);
    out
}

/// The first argument, if it is a quoted string — a `DateTime`'s zone.
fn quoted_arg(args: Option<&str>) -> Option<&str> {
    quoted(top_level(args?).first()?.trim())
}

fn quoted(text: &str) -> Option<&str> {
    text.strip_prefix('\'')?.strip_suffix('\'')
}

fn decimal_width(args: Option<&str>) -> Option<(u8, i8)> {
    let parts = top_level(args?);
    let precision = parts.first()?.trim().parse().ok()?;
    let scale = parts.get(1)?.trim().parse().ok()?;
    Some((precision, scale))
}

/// Wraps a name in backticks, the way ClickHouse's own parser reads one back.
///
/// Backslash escaping and not doubling: `` `we\`ird` `` is how ClickHouse
/// spells a backtick inside an identifier, which is only discoverable by asking
/// it — SQL's usual answer is to double the delimiter, and that would produce a
/// different name here without failing.
pub(crate) fn quote_identifier(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('`');
    for c in name.chars() {
        if c == '`' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('`');
    out
}

fn escape_literal(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule this module exists to keep. Every type ClickHouse can declare
    /// has to land in the set the Arrow reader on the other side of the FFI has
    /// a case for; a column that arrives as anything else is drawn as its own
    /// format string in every cell, which is not a column.
    #[test]
    fn every_type_lands_somewhere_the_grid_can_draw() {
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
        ];
        for declared in [
            "Bool",
            "Int8",
            "Int16",
            "Int32",
            "Int64",
            "Int128",
            "Int256",
            "UInt8",
            "UInt16",
            "UInt32",
            "UInt64",
            "UInt128",
            "UInt256",
            "Float32",
            "Float64",
            "BFloat16",
            "Decimal(9, 4)",
            "Decimal(76, 40)",
            "Date",
            "Date32",
            "DateTime",
            "DateTime('Asia/Taipei')",
            "DateTime64(3)",
            "DateTime64(9, 'Asia/Taipei')",
            "Enum8('draft' = -1)",
            "Enum16('alpha' = 1000)",
            "LowCardinality(String)",
            "LowCardinality(Nullable(String))",
            "Nullable(Int32)",
            "Array(Int32)",
            "Array(Array(String))",
            "Tuple(Int32, String)",
            "Tuple(\n    qty Int32,\n    unit String)",
            "Map(String, Array(Int64))",
            "FixedString(8)",
            "String",
            "UUID",
            "IPv4",
            "IPv6",
            "JSON",
            "Point",
            "IntervalDay",
            "Variant(Int32, String)",
            "Dynamic",
            "Nested(a Int32, b String)",
            "AggregateFunction(sum, UInt64)",
            "SomethingClickHouseAddedLastTuesday",
        ] {
            let arrow = map(declared, "UTC").arrow;
            let fine = readable.contains(&arrow)
                || matches!(arrow, DataType::Decimal128(..))
                || matches!(arrow, DataType::Timestamp(TimeUnit::Microsecond, _));
            assert!(
                fine,
                "{declared} would arrive as {arrow:?}, which the grid draws as a format string"
            );
        }
    }

    /// The wrappers are not types and must not decide the mapping — but one of
    /// them decides the nullability, and it can be the inner one.
    #[test]
    fn low_cardinality_does_not_hide_a_nullable() {
        let plain = map("LowCardinality(String)", "UTC");
        assert!(!plain.nullable);
        assert_eq!(plain.arrow, DataType::Utf8);

        let nullable = map("LowCardinality(Nullable(String))", "UTC");
        assert!(nullable.nullable);
        assert_eq!(nullable.arrow, DataType::Utf8);

        // The wrapper must not change what the column is, either: a
        // LowCardinality integer is still an integer.
        assert_eq!(
            map("LowCardinality(UInt32)", "UTC").arrow,
            DataType::Int64,
            "the mapping should see through the wrapper to UInt32"
        );
    }

    /// A `UInt64` holds numbers no signed Arrow integer does, and the one that
    /// proves it is the one people paste into a bug report.
    #[test]
    fn a_uint64_gets_a_type_that_holds_all_of_it() {
        let mapping = map("UInt64", "UTC");
        assert_eq!(mapping.arrow, DataType::Decimal128(20, 0));
        // 20 digits is exactly u64::MAX's width; 19 would truncate it.
        assert_eq!(u64::MAX.to_string().len(), 20);
    }

    /// The zone is in the declared type and nowhere in the Arrow field
    /// ClickHouse sends, so losing it here loses it for good.
    #[test]
    fn a_datetime_keeps_the_zone_it_was_declared_with() {
        let taipei = map("DateTime('Asia/Taipei')", "UTC");
        assert_eq!(
            taipei.arrow,
            DataType::Timestamp(TimeUnit::Microsecond, Some("Asia/Taipei".into()))
        );
        assert_eq!(
            taipei.cast,
            Some(Cast::Micros("Asia/Taipei".to_string())),
            "the projection has to name the zone, or the server picks its own"
        );

        // No zone of its own means the server's, which is why the server's is
        // read at connect rather than assumed.
        let bare = map("DateTime", "Asia/Taipei");
        assert_eq!(
            bare.arrow,
            DataType::Timestamp(TimeUnit::Microsecond, Some("Asia/Taipei".into()))
        );

        let sub_second = map("DateTime64(6, 'Asia/Taipei')", "UTC");
        assert_eq!(
            sub_second.arrow,
            DataType::Timestamp(TimeUnit::Microsecond, Some("Asia/Taipei".into()))
        );
        // Nine digits do not fit in six, and dropping three of them quietly is
        // worse than handing over all nine as text.
        assert_eq!(map("DateTime64(9)", "UTC").arrow, DataType::Utf8);
    }

    /// A statement over ordinary columns must be sent as the caller wrote it.
    /// Wrapping it costs a subquery, and — because ClickHouse collapses columns
    /// that share a name — is not always safe.
    #[test]
    fn a_plain_statement_is_not_wrapped() {
        let plan = plan(
            &[
                ("a".to_string(), "Int32".to_string()),
                ("b".to_string(), "String".to_string()),
                ("c".to_string(), "Nullable(Float64)".to_string()),
            ],
            "UTC",
            false,
        );
        assert!(plan.select_list.is_none());
        assert_eq!(plan.schema.field(2).data_type(), &DataType::Float64);
        assert!(plan.schema.field(2).is_nullable());
        assert!(!plan.schema.field(0).is_nullable());
    }

    #[test]
    fn a_statement_with_a_uuid_is_asked_for_differently() {
        let plan = plan(
            &[
                ("id".to_string(), "Int32".to_string()),
                ("uid".to_string(), "UUID".to_string()),
            ],
            "UTC",
            false,
        );
        assert_eq!(
            plan.select_list.as_deref(),
            Some("`id`, toString(`uid`) AS `uid`")
        );
        assert_eq!(plan.schema.field(1).data_type(), &DataType::Utf8);
    }

    /// The retry changes the SQL and not the schema, which is the only reason it
    /// can happen behind a caller that has already been handed a schema.
    #[test]
    fn the_retry_keeps_the_shape_it_promised() {
        let columns = [
            ("id".to_string(), "Int32".to_string()),
            ("s".to_string(), "String".to_string()),
        ];
        let first = plan(&columns, "UTC", false);
        let second = plan(&columns, "UTC", true);
        assert_eq!(first.schema, second.schema);
        assert_eq!(
            second.select_list.as_deref(),
            Some("`id`, toValidUTF8(toString(`s`)) AS `s`")
        );
    }

    /// Doubling the delimiter is the SQL habit and produces a different name
    /// here, silently.
    #[test]
    fn an_identifier_is_escaped_the_way_the_server_reads_it_back() {
        assert_eq!(quote_identifier("plain"), "`plain`");
        assert_eq!(quote_identifier("we`ird"), "`we\\`ird`");
        assert_eq!(quote_identifier("back\\slash"), "`back\\\\slash`");
    }

    /// A named tuple comes back from `DESCRIBE` across several lines, and a
    /// parser that split on the first newline would see a type called `Tuple(`.
    #[test]
    fn a_type_printed_across_lines_is_still_one_type() {
        let declared = "Tuple(\n    qty Int32,\n    unit String)";
        assert_eq!(map(declared, "UTC").arrow, DataType::Utf8);
        assert_eq!(split(declared).0, "Tuple");
    }

    #[test]
    fn arguments_are_split_outside_nesting_and_quotes() {
        assert_eq!(
            top_level("String, Array(Int64)"),
            ["String", " Array(Int64)"]
        );
        assert_eq!(top_level("9, 'Asia/Taipei'"), ["9", " 'Asia/Taipei'"]);
        assert_eq!(top_level("'a,b' = 1, 'c' = 2"), ["'a,b' = 1", " 'c' = 2"]);
    }
}
