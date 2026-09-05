//! What DuckDB's Arrow has to become before a grid can show it.
//!
//! The other drivers build Arrow arrays a value at a time, so their `arrow_map`
//! is a table of database type to Arrow type. DuckDB produces the arrays itself
//! and this file has no conversion left to do — a `DECIMAL(18,6)` arrives as
//! `Decimal128(18, 6)` with the declared precision and scale already on the
//! field, where the PostgreSQL driver spends twenty-five lines and four tests
//! unpacking a type modifier to reach the same answer.
//!
//! What is left is the opposite problem. Everything arrives, and the question is
//! whether the front end can render it. Two things happen here.
//!
//! **Settings that decide a column's Arrow type are pinned.** Five of them, all
//! set to DuckDB 1.5.5's current defaults. Pinning them costs nothing and turns a
//! DuckDB upgrade that changes a default into a build-time decision rather than
//! into a column that renders as a format string.
//!
//! **Columns the reader can only read through their children are rendered to
//! text.** `apps/macos/Sources/DbClient/ArrowTable.swift` maps a closed set of
//! Arrow format strings, and a column it has no case for reaches the grid as
//! `<+s>` in every cell — not a value, not a null, the format string itself.
//! DuckDB is the driver most likely to produce one: `STRUCT`, `LIST`, `MAP`,
//! `ARRAY(n)`, `UNION` and `ENUM` are ordinary DuckDB types, not extensions.
//!
//! Four of those six the reader now follows. It grew a walk into `children` for
//! `+l`, `+s`, `+m` and `+w:N` on the day a Flight SQL server sent nesting no
//! driver had flattened — there being no `arrow_map.rs` in that driver, nor in
//! BigQuery's or Databricks's, to flatten it in. So this file's rendering is no
//! longer the only thing standing between a `STRUCT` and a format string; it is
//! now a *second* rendering of the same value, and the two disagree: a DuckDB
//! struct reaches the grid here as `{qty: 2, unit: kg}`, DuckDB's own spelling,
//! and reaches it over Flight SQL — which is this same DuckDB, behind a protocol
//! — as `{"qty":2,"unit":"kg"}`. That is a conflict rather than a preference, and
//! the way out of it is to stop rendering the four here and let the reader read
//! them, which would also give the value viewer a `STRUCT` it can lay out instead
//! of a line. It is left standing because deleting a rendering is a change to
//! what every DuckDB user already sees, and it belongs in its own round with its
//! own screenshot. `UNION`, `ENUM` and the list views below are not in that
//! argument: nothing follows those, and this is the only thing between them and
//! `<+ud:>`.
//!
//! The line is drawn at whether the column's own buffers hold the value.
//! `UBIGINT` arrives as `UInt64`, which the reader also has no case for — but the
//! bytes in that buffer *are* the number, so the fix is one `case` in Swift and
//! turning it into text here would throw away a number to work around a missing
//! line of Swift. A `STRUCT`'s buffers hold nothing but a validity bitmap; the
//! values are in children the reader does not follow, so there is nothing to
//! preserve by leaving it alone. Those become `Utf8`, rendered by arrow-rs the
//! way DuckDB's own shell renders them — `{qty: 2, unit: kg}`, `[1, 2, 3]`.

use arrow::array::{Array, ArrayRef, RecordBatch, StringBuilder, new_empty_array};
use arrow::datatypes::{DataType, Field, FieldRef, Schema, SchemaRef};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use std::fmt::Write as _;
use std::sync::Arc;

use crate::DuckError;

/// Field metadata recording what a text column used to be.
///
/// Nothing reads it yet. It is written because a schema that says `Utf8` and
/// nothing else is a schema claiming DuckDB returned a string, and the one thing
/// a type label must not do is state something the column was never declared
/// with. The grid has since grown a nested renderer, and this is still where the
/// undoing of this file starts: it is the only record of what each rendered
/// column was, and so the only way to tell a `STRUCT` that was flattened here
/// from a `VARCHAR` that was always one.
pub const RENDERED_FROM: &str = "duckdb.rendered_from";

/// Settings that change the Arrow type of a column, fixed at DuckDB 1.5.5's
/// defaults for the length of a connection.
///
/// Each one of these silently moves a column to a different `DataType`:
/// `Utf8` to `LargeUtf8` or `Utf8View`, `List` to `ListView`, and
/// `arrow_lossless_conversion` retags `BOOLEAN`, `HUGEINT`, `UUID`, `TIME_TZ`
/// and `JSON` as Arrow extension types. The last one is the tempting one — it
/// would recover in the schema most of what §4's type table says Arrow throws
/// away — and it is off because it also changes the physical layout to something
/// the grid does not read, and because `ENUM` is documented as not tagged even
/// then.
pub const PINNED_SETTINGS: &str = "\
SET arrow_large_buffer_size = false;
SET produce_arrow_string_view = false;
SET arrow_output_list_view = false;
SET arrow_output_version = '1.0';
SET arrow_lossless_conversion = false;";

/// How one result's columns reach the front end.
///
/// Decided once from the schema, before the first batch, so that a result the
/// grid could not show fails at `query` rather than three pages in.
pub struct Layout {
    schema: SchemaRef,
    /// `true` where the column is rendered to text. Indexed with the schema.
    as_text: Vec<bool>,
    any_text: bool,
}

impl Layout {
    /// Reads `schema` and settles what each column becomes.
    ///
    /// Fails only where a column has to be rendered to text and arrow-rs cannot
    /// render it — today that is a `UNION` containing a zoned timestamp, which is
    /// checked here with an empty array rather than discovered on the first
    /// batch.
    pub fn of(schema: &Schema) -> Result<Self, DuckError> {
        let mut fields: Vec<FieldRef> = Vec::with_capacity(schema.fields().len());
        let mut as_text = Vec::with_capacity(schema.fields().len());
        let mut any_text = false;

        for field in schema.fields() {
            if !reaches_the_grid_through_children(field.data_type()) {
                fields.push(Arc::clone(field));
                as_text.push(false);
                continue;
            }
            let rendered = renderable(field.data_type());
            let probe = new_empty_array(&rendered);
            if ArrayFormatter::try_new(&probe, &FORMAT).is_err() {
                return Err(DuckError::Unreadable {
                    column: field.name().clone(),
                    arrow_type: format!("{}", field.data_type()),
                });
            }
            any_text = true;
            as_text.push(true);
            fields.push(Arc::new(
                Field::new(field.name(), DataType::Utf8, field.is_nullable()).with_metadata(
                    [(RENDERED_FROM.to_string(), format!("{}", field.data_type()))].into(),
                ),
            ));
        }

        Ok(Self {
            schema: Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone())),
            as_text,
            any_text,
        })
    }

    /// The columns a caller will actually receive.
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rewrites one batch into that shape.
    ///
    /// Returns the batch untouched where nothing needs rendering, which is every
    /// result that has no nested column — the case worth keeping free, since it
    /// is the one the throughput numbers are measured on.
    pub fn apply(&self, batch: RecordBatch) -> Result<RecordBatch, DuckError> {
        if !self.any_text {
            return Ok(batch);
        }
        let columns = batch
            .columns()
            .iter()
            .zip(&self.as_text)
            .map(|(column, text)| {
                if *text {
                    to_text(column)
                } else {
                    Ok(Arc::clone(column))
                }
            })
            .collect::<Result<Vec<ArrayRef>, DuckError>>()?;
        Ok(RecordBatch::try_new(Arc::clone(&self.schema), columns)?)
    }
}

/// Null renders as the empty string rather than as `NULL`, because the validity
/// bitmap survives into the text column and the front end reads nulls from
/// there. Spelling the word into the cell would make a genuine `'NULL'` string
/// and a SQL NULL look identical.
const FORMAT: FormatOptions<'static> = FormatOptions::new().with_null("");

/// Whether the front end would have to follow this column's children to show a
/// value.
///
/// The nested types, plus `Dictionary` — an `ENUM` arrives as
/// `Dictionary(UInt8, Utf8)`, whose format string is the *index* type, so the
/// reader would show `<C>` and the buffer it reads holds dictionary indices
/// rather than labels. Everything else keeps its own values in its own buffers,
/// including the several types the reader has no case for yet.
fn reaches_the_grid_through_children(t: &DataType) -> bool {
    matches!(
        t,
        DataType::Struct(_)
            | DataType::List(_)
            | DataType::LargeList(_)
            | DataType::ListView(_)
            | DataType::LargeListView(_)
            | DataType::FixedSizeList(..)
            | DataType::Map(..)
            | DataType::Union(..)
            | DataType::Dictionary(..)
            | DataType::RunEndEncoded(..)
    )
}

/// The same type with named time zones replaced by none.
///
/// arrow-rs refuses to format `Timestamp(_, Some("Asia/Taipei"))` without its
/// `chrono-tz` feature, and DuckDB puts the session time zone — a name, not an
/// offset — on every `TIMESTAMPTZ`. Dropping the name loses the label and not the
/// instant, which for a value being rendered into a cell inside a struct is the
/// smaller loss; the alternative is a megabyte of time zone tables linked into
/// every build of the workspace so that one nested column can print a suffix.
///
/// `Union` is deliberately not descended into: arrow-rs has no union-to-union
/// cast, so there would be nothing to do with the rewritten type. A `UNION` of
/// zoned timestamps is refused by `Layout::of` instead.
fn renderable(t: &DataType) -> DataType {
    let child = |f: &FieldRef| {
        Arc::new(Field::new(
            f.name(),
            renderable(f.data_type()),
            f.is_nullable(),
        ))
    };
    match t {
        DataType::Timestamp(unit, Some(_)) => DataType::Timestamp(*unit, None),
        DataType::Struct(fields) => DataType::Struct(fields.iter().map(child).collect()),
        DataType::List(f) => DataType::List(child(f)),
        DataType::LargeList(f) => DataType::LargeList(child(f)),
        DataType::FixedSizeList(f, len) => DataType::FixedSizeList(child(f), *len),
        DataType::Map(f, sorted) => DataType::Map(child(f), *sorted),
        DataType::Dictionary(key, value) => {
            DataType::Dictionary(key.clone(), Box::new(renderable(value)))
        }
        other => other.clone(),
    }
}

/// One column, rendered the way DuckDB's own shell would render it.
fn to_text(column: &ArrayRef) -> Result<ArrayRef, DuckError> {
    let wanted = renderable(column.data_type());
    let source = if wanted == *column.data_type() {
        Arc::clone(column)
    } else {
        arrow::compute::kernels::cast::cast(column, &wanted)?
    };

    let formatter = ArrayFormatter::try_new(&source, &FORMAT)?;
    let mut builder = StringBuilder::with_capacity(source.len(), source.len() * 24);
    // One buffer reused across cells rather than a `String` per value: this runs
    // over every row of a nested column, and the formatter writes into anything
    // that takes `fmt::Write`.
    let mut cell = String::new();
    for row in 0..source.len() {
        if source.is_null(row) {
            builder.append_null();
            continue;
        }
        cell.clear();
        write!(cell, "{}", formatter.value(row))
            .map_err(|e| DuckError::Arrow(arrow::error::ArrowError::CastError(e.to_string())))?;
        builder.append_value(&cell);
    }
    Ok(Arc::new(builder.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int32Array, Int32Builder, ListBuilder, StringArray, StructArray};
    use arrow::datatypes::{Fields, TimeUnit};

    fn field(name: &str, t: DataType) -> Field {
        Field::new(name, t, true)
    }

    #[test]
    fn a_column_whose_buffers_hold_its_own_values_is_left_alone() {
        // Every one of these is a type the Swift reader has no case for. Turning
        // them into text would remove the value to work around a missing case:
        // the bytes in the buffer already are the number.
        for t in [
            DataType::Int8,
            DataType::UInt64,
            DataType::Interval(arrow::datatypes::IntervalUnit::MonthDayNano),
            DataType::Timestamp(TimeUnit::Nanosecond, None),
            DataType::Time64(TimeUnit::Nanosecond),
            DataType::Null,
        ] {
            assert!(
                !reaches_the_grid_through_children(&t),
                "{t} should reach the grid as itself"
            );
        }
    }

    #[test]
    fn a_column_the_reader_could_only_follow_through_children_is_rendered() {
        for t in [
            DataType::Struct(Fields::from(vec![field("a", DataType::Int32)])),
            DataType::List(Arc::new(field("l", DataType::Int32))),
            DataType::FixedSizeList(Arc::new(field("", DataType::Int32)), 3),
            DataType::Map(
                Arc::new(field(
                    "entries",
                    DataType::Struct(Fields::from(vec![
                        Field::new("key", DataType::Utf8, false),
                        field("value", DataType::Int32),
                    ])),
                )),
                false,
            ),
            // An ENUM. Not nested in the C data interface, but its format string
            // is the index type and its buffer holds indices, so the reader would
            // show `<C>` over a column of numbers.
            DataType::Dictionary(Box::new(DataType::UInt8), Box::new(DataType::Utf8)),
        ] {
            assert!(
                reaches_the_grid_through_children(&t),
                "{t} would reach the grid as its format string"
            );
        }
    }

    #[test]
    fn a_struct_arrives_as_the_text_duckdbs_own_shell_would_print() {
        let inner = StructArray::from(vec![
            (
                Arc::new(field("qty", DataType::Int32)),
                Arc::new(Int32Array::from(vec![Some(2), None])) as ArrayRef,
            ),
            (
                Arc::new(field("unit", DataType::Utf8)),
                Arc::new(StringArray::from(vec![Some("kg"), None])) as ArrayRef,
            ),
        ]);
        let schema = Schema::new(vec![field("v", inner.data_type().clone())]);
        let layout = Layout::of(&schema).unwrap();
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(inner)]).unwrap();

        let out = layout.apply(batch).unwrap();
        assert_eq!(out.schema().field(0).data_type(), &DataType::Utf8);
        let text = out
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(text.value(0), "{qty: 2, unit: kg}");
        assert_eq!(text.value(1), "{qty: , unit: }");
    }

    #[test]
    fn a_rendered_column_says_what_it_used_to_be() {
        let schema = Schema::new(vec![field(
            "v",
            DataType::List(Arc::new(field("l", DataType::Int32))),
        )]);
        let layout = Layout::of(&schema).unwrap();
        let metadata = layout.schema().field(0).metadata().clone();
        // A schema that said `Utf8` and nothing else would be claiming DuckDB
        // returned a string.
        assert!(metadata[RENDERED_FROM].starts_with("List"));
    }

    #[test]
    fn a_null_in_a_rendered_column_stays_a_null() {
        let mut builder = ListBuilder::new(Int32Builder::new());
        builder.values().append_value(1);
        builder.append(true);
        builder.append(false);
        let list = builder.finish();

        let schema = Schema::new(vec![field("v", list.data_type().clone())]);
        let layout = Layout::of(&schema).unwrap();
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(list)]).unwrap();
        let out = layout.apply(batch).unwrap();

        // Rendering the word NULL into the cell would make a genuine 'NULL'
        // string and a SQL NULL indistinguishable in the grid.
        assert!(!out.column(0).is_null(0));
        assert!(out.column(0).is_null(1));
    }

    #[test]
    fn a_zoned_timestamp_inside_a_struct_keeps_its_instant() {
        use arrow::array::TimestampMicrosecondArray;
        let zoned = TimestampMicrosecondArray::from(vec![1_704_067_200_000_000i64])
            .with_timezone("Asia/Taipei");
        let inner = StructArray::from(vec![(
            Arc::new(field("when", zoned.data_type().clone())),
            Arc::new(zoned) as ArrayRef,
        )]);
        let schema = Schema::new(vec![field("v", inner.data_type().clone())]);
        let layout = Layout::of(&schema).unwrap();
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(inner)]).unwrap();

        // arrow-rs cannot format a named zone without chrono-tz, so the zone is
        // dropped and the instant kept. Refusing the whole result over a suffix
        // would be the worse trade.
        let out = layout.apply(batch).unwrap();
        let text = out
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(text.value(0), "{when: 2024-01-01T00:00:00}");
    }

    #[test]
    fn a_column_that_cannot_be_rendered_is_refused_before_the_first_batch() {
        use arrow::datatypes::{UnionFields, UnionMode};
        // arrow-rs has no union-to-union cast, so the zone cannot be dropped the
        // way it is for the other containers, and the formatter refuses a named
        // one. Better here, where the message can name the column and say to
        // cast it, than three pages into the result.
        let union = DataType::Union(
            UnionFields::try_new(
                [0],
                [field(
                    "when",
                    DataType::Timestamp(TimeUnit::Microsecond, Some("Asia/Taipei".into())),
                )],
            )
            .unwrap(),
            UnionMode::Sparse,
        );
        let err = Layout::of(&Schema::new(vec![field("v", union)]))
            .err()
            .expect("a column the grid could not be given");
        let message = err.to_string();
        assert!(
            message.contains("\"v\""),
            "it should name the column: {message}"
        );
        assert!(message.contains("CAST"), "and say what to write: {message}");
    }

    #[test]
    fn a_result_with_nothing_nested_in_it_is_not_copied() {
        let schema = Arc::new(Schema::new(vec![field("id", DataType::Int64)]));
        let layout = Layout::of(&schema).unwrap();
        assert_eq!(layout.schema().as_ref(), schema.as_ref());

        let column: ArrayRef = Arc::new(arrow::array::Int64Array::from(vec![1, 2, 3]));
        let before = column.to_data().buffers()[0].as_ptr();
        let batch = RecordBatch::try_new(Arc::clone(&schema), vec![column]).unwrap();
        let after = layout.apply(batch).unwrap().column(0).to_data().buffers()[0].as_ptr();
        assert_eq!(
            before, after,
            "a plain result should pass through untouched"
        );
    }
}
