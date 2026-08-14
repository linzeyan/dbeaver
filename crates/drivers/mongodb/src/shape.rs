//! Giving a collection of documents a single set of columns.
//!
//! This is the file the phase exists to produce. Every other driver is handed a
//! schema by the database before the first row: PostgreSQL describes the result,
//! SQLite settles a column's type once the first row is stepped. MongoDB has no
//! such answer to give. Two documents in one collection may share not one field,
//! and the `ResultStream` contract requires the columns to be known *before* the
//! first batch and to stay the same for the whole result.
//!
//! So the columns are inferred from a sample, and the interesting part is what
//! happens to a document that does not fit the inference.
//!
//! **The rule.** The schema is the union of top-level field names seen in a
//! sample, in the order they were first seen, each typed by reconciling the types
//! its values had. A field absent from a document is null, which is what a
//! nullable column is for. A field *outside* the schema — one the sample never
//! saw — goes into a trailing `_extra` column holding the leftover fields as JSON
//! text.
//!
//! **Why `_extra` and not simply dropping it.** Dropping is silent data loss in a
//! tool whose entire job is showing what is in the database, and it is the kind
//! that is never noticed: the row is there, the value is not, and nothing says
//! so. Failing the batch instead would make a ragged collection unreadable.
//!
//! **Why it is conditional.** A column that is empty in every row of every query
//! is clutter, and most collections in practice are uniform because an
//! application wrote them. So `_extra` is added only when the sample itself found
//! disagreement — more than one distinct set of field names. That is decidable
//! from the sample, which is what matters: the schema still has to be fixed
//! before the first document is delivered.
//!
//! **Why top-level only.** Flattening `a.b.c` into columns reads well on the
//! documents that have it and explodes into hundreds of mostly-null columns on
//! the documents that do not. A nested document or array is one column of JSON
//! text, which is the form a person reads it in anyway.

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Float64Builder, Int32Builder, Int64Builder,
    RecordBatch, StringBuilder, TimestampMillisecondBuilder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use bson::{Bson, Document};
use std::sync::Arc;

use crate::MongoError;

/// The column `_extra` goes in, named with the leading underscore MongoDB itself
/// uses for `_id` so it reads as belonging to the storage rather than to the
/// data.
pub const EXTRA: &str = "_extra";

/// What a column's values are, before Arrow.
///
/// Kept separate from `DataType` because the reconciliation below is a lattice
/// over *these* — Arrow has a hundred types and reconciling over all of them
/// would mean deciding what a `Duration` and a `LargeUtf8` have in common, which
/// is a question no BSON document asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bool,
    Int32,
    Int64,
    Float64,
    /// Milliseconds since the epoch, UTC. BSON's date is exactly this and
    /// carries no zone of its own.
    DateTime,
    Binary,
    /// The catch-all, and the honest one. An ObjectId is its 24 hex digits, a
    /// nested document is its JSON, a `Decimal128` is its digits — see
    /// `type_of` for why each ended up here.
    Text,
}

impl ColumnType {
    fn arrow(self) -> DataType {
        match self {
            ColumnType::Bool => DataType::Boolean,
            ColumnType::Int32 => DataType::Int32,
            ColumnType::Int64 => DataType::Int64,
            ColumnType::Float64 => DataType::Float64,
            ColumnType::DateTime => DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            ColumnType::Binary => DataType::Binary,
            ColumnType::Text => DataType::Utf8,
        }
    }

    /// The type that can hold values of both, which is `Text` unless the two are
    /// numbers that widen into one another.
    ///
    /// Widening only where nothing is lost on the way. `Int32` into `Int64` is
    /// exact. `Int32` into `Float64` is exact, since a 32-bit integer fits a
    /// double's 53-bit mantissa. `Int64` into `Float64` is *not* — which is why
    /// a field holding both large integers and doubles becomes text rather than
    /// quietly rounding an account number.
    fn unify(self, other: ColumnType) -> ColumnType {
        use ColumnType::*;
        match (self, other) {
            (a, b) if a == b => a,
            (Int32, Int64) | (Int64, Int32) => Int64,
            (Int32, Float64) | (Float64, Int32) => Float64,
            _ => Text,
        }
    }
}

/// What a single BSON value is, or `None` for null and undefined — which say
/// nothing about the column's type and must not drag it to `Text`.
///
/// The cases that landed on `Text` are decisions rather than omissions:
///
/// - **ObjectId** is 12 bytes that everyone, including MongoDB's own shell,
///   reads and writes as 24 hex digits. `Binary` would be correct and unusable.
/// - **Decimal128** is IEEE 754-2008 decimal: 34 significant digits with an
///   exponent that moves per value. Arrow's `Decimal128` fixes precision and
///   scale for the whole column, so mapping onto it means choosing a scale on
///   the strength of a sample and silently rescaling every value that disagrees.
///   For a type whose entire purpose is money, the digits as written are the
///   only safe answer. This is the sharpest loss in this file.
/// - **Document and Array** are the top-level-only rule above.
/// - **Regex, JavaScript, Symbol, DbPointer, MinKey, MaxKey, Timestamp** are
///   either operational internals or have no numeric or temporal reading. BSON's
///   `Timestamp` in particular is a replication counter, not a date, and showing
///   it as one would put a 1970 date beside every record.
fn type_of(value: &Bson) -> Option<ColumnType> {
    match value {
        Bson::Null | Bson::Undefined => None,
        Bson::Boolean(_) => Some(ColumnType::Bool),
        Bson::Int32(_) => Some(ColumnType::Int32),
        Bson::Int64(_) => Some(ColumnType::Int64),
        Bson::Double(_) => Some(ColumnType::Float64),
        Bson::DateTime(_) => Some(ColumnType::DateTime),
        Bson::Binary(_) => Some(ColumnType::Binary),
        _ => Some(ColumnType::Text),
    }
}

/// A value rendered for a `Text` column.
///
/// Not `Bson::to_string`, which produces Extended JSON — `{"$oid": "..."}`
/// around an ObjectId and `{"$numberLong": "5"}` around an integer. That form
/// exists so a document survives a round trip through JSON, and it is the wrong
/// thing to put in a grid cell: the reader wants the id, not the tagging that
/// preserves the id's type.
fn text_of(value: &Bson) -> String {
    match value {
        Bson::String(s) => s.clone(),
        Bson::ObjectId(id) => id.to_hex(),
        Bson::Decimal128(d) => d.to_string(),
        Bson::Boolean(b) => b.to_string(),
        Bson::Int32(i) => i.to_string(),
        Bson::Int64(i) => i.to_string(),
        Bson::Double(f) => f.to_string(),
        Bson::DateTime(d) => d.try_to_rfc3339_string().unwrap_or_else(|_| d.to_string()),
        // A document or array is shown as ordinary JSON for the same reason:
        // `{"n": 5}` is what the reader recognises, `{"n": {"$numberInt": "5"}}`
        // is what a parser needs.
        other => serde_json::to_string(&plain_json(other))
            .unwrap_or_else(|_| "<unrenderable>".to_string()),
    }
}

/// BSON as the JSON a person would write, with the type tags dropped.
///
/// Deliberately lossy and only ever used for display. Extended JSON's tags are
/// there so a reader can tell an ObjectId from the string of its digits; a grid
/// cell has no such need, and carrying them turns every nested document into
/// something that has to be read past rather than read.
fn plain_json(value: &Bson) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Boolean(b) => Value::Bool(*b),
        Bson::Int32(i) => Value::from(*i),
        Bson::Int64(i) => Value::from(*i),
        Bson::Double(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            // NaN and the infinities are values BSON holds and JSON has no
            // spelling for. Their names are better than a null that claims the
            // field was absent.
            .unwrap_or_else(|| Value::String(f.to_string())),
        Bson::String(s) => Value::String(s.clone()),
        Bson::ObjectId(id) => Value::String(id.to_hex()),
        Bson::DateTime(d) => {
            Value::String(d.try_to_rfc3339_string().unwrap_or_else(|_| d.to_string()))
        }
        Bson::Decimal128(d) => Value::String(d.to_string()),
        Bson::Array(items) => Value::Array(items.iter().map(plain_json).collect()),
        Bson::Document(doc) => Value::Object(
            doc.iter()
                .map(|(k, v)| (k.clone(), plain_json(v)))
                .collect(),
        ),
        Bson::Binary(b) => Value::String(format!("<{} bytes>", b.bytes.len())),
        other => Value::String(other.to_string()),
    }
}

/// The columns a result will have, and where each document's leftovers go.
#[derive(Debug, Clone)]
pub struct Shape {
    /// Field names in the order they were first seen, `_extra` excluded.
    names: Vec<String>,
    types: Vec<ColumnType>,
    /// Whether the trailing `_extra` column is present — see the module note on
    /// why this is decided from the sample rather than per batch.
    extra: bool,
    schema: SchemaRef,
}

impl Shape {
    /// Infers the columns of a result from documents drawn from it.
    ///
    /// An empty sample is an empty collection or a query matching nothing, and
    /// the honest schema for that is no columns at all: inventing `_id` would
    /// state that a collection has a field when nothing has established that it
    /// has any documents.
    pub fn infer(sample: &[Document]) -> Shape {
        let mut names: Vec<String> = Vec::new();
        // `None` is "seen, but only ever null", which is not the same as `Text`
        // and must not be: a field whose first document holds null would
        // otherwise be locked to text, and every later integer in it would
        // unify against text and stay there.
        let mut types: Vec<Option<ColumnType>> = Vec::new();
        // Compared as sorted name lists rather than as sets, so that two
        // documents with the same fields in a different order are the same
        // shape — key order in BSON is preserved on the wire and means nothing
        // here.
        let mut layouts: Vec<Vec<&str>> = Vec::new();

        for document in sample {
            let mut layout: Vec<&str> = Vec::new();
            for (key, value) in document {
                layout.push(key.as_str());
                match names.iter().position(|n| n == key) {
                    Some(at) => {
                        if let Some(found) = type_of(value) {
                            types[at] = Some(match types[at] {
                                Some(known) => known.unify(found),
                                None => found,
                            });
                        }
                    }
                    None => {
                        names.push(key.clone());
                        // A field first seen holding null is still a field, but
                        // nothing is known about it yet.
                        types.push(type_of(value));
                    }
                }
            }
            layout.sort_unstable();
            if !layouts.contains(&layout) {
                layouts.push(layout);
            }
        }

        let extra = layouts.len() > 1;
        // A field that was null in every document sampled has no type to report.
        // Text is the right answer: every value in the column is null, so the
        // choice costs nothing, and it is the type that can hold whatever turns
        // up in a document the sample did not reach.
        let types = types
            .into_iter()
            .map(|t| t.unwrap_or(ColumnType::Text))
            .collect();
        Shape::build(names, types, extra)
    }

    fn build(names: Vec<String>, types: Vec<ColumnType>, extra: bool) -> Shape {
        let mut fields: Vec<Field> = names
            .iter()
            .zip(&types)
            // Every column is nullable without exception. A field present in
            // every sampled document may still be missing from the next one, and
            // a schema that promised otherwise would be a promise this database
            // cannot keep.
            .map(|(name, ty)| Field::new(name, ty.arrow(), true))
            .collect();
        if extra {
            fields.push(Field::new(EXTRA, DataType::Utf8, true));
        }
        let schema = Arc::new(Schema::new(fields));
        Shape {
            names,
            types,
            extra,
            schema,
        }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// The column names, for the structure pane. `_extra` is included when it is
    /// present: it is a column the grid will show, so hiding it here would make
    /// the two disagree.
    pub fn columns(&self) -> Vec<(String, ColumnType)> {
        let mut out: Vec<(String, ColumnType)> = self
            .names
            .iter()
            .cloned()
            .zip(self.types.iter().copied())
            .collect();
        if self.extra {
            out.push((EXTRA.to_string(), ColumnType::Text));
        }
        out
    }

    /// Packs documents into one batch of this shape.
    pub fn batch(&self, documents: &[Document]) -> Result<RecordBatch, MongoError> {
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(self.names.len() + 1);
        for (at, ty) in self.types.iter().enumerate() {
            columns.push(self.column(&self.names[at], *ty, documents));
        }
        if self.extra {
            columns.push(self.leftovers(documents));
        }
        RecordBatch::try_new(self.schema(), columns).map_err(MongoError::Arrow)
    }

    fn column(&self, name: &str, ty: ColumnType, documents: &[Document]) -> ArrayRef {
        let rows = documents.len();
        // `Bson::Null` and an absent field are both null here, and deliberately
        // not distinguished. MongoDB does distinguish them and queries can tell
        // them apart, but a grid has one empty cell and inventing a second
        // rendering for "explicitly null" would be showing the reader a
        // difference they cannot act on.
        let values = documents.iter().map(|d| d.get(name));

        match ty {
            ColumnType::Bool => {
                let mut b = BooleanBuilder::with_capacity(rows);
                for v in values {
                    match v {
                        Some(Bson::Boolean(x)) => b.append_value(*x),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            ColumnType::Int32 => {
                let mut b = Int32Builder::with_capacity(rows);
                for v in values {
                    match v {
                        Some(Bson::Int32(x)) => b.append_value(*x),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            ColumnType::Int64 => {
                let mut b = Int64Builder::with_capacity(rows);
                for v in values {
                    match v {
                        Some(Bson::Int64(x)) => b.append_value(*x),
                        // The widening `unify` promised: a field holding both
                        // sizes is one column of the larger, so the 32-bit
                        // values have to be lifted here too.
                        Some(Bson::Int32(x)) => b.append_value(i64::from(*x)),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            ColumnType::Float64 => {
                let mut b = Float64Builder::with_capacity(rows);
                for v in values {
                    match v {
                        Some(Bson::Double(x)) => b.append_value(*x),
                        Some(Bson::Int32(x)) => b.append_value(f64::from(*x)),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            ColumnType::DateTime => {
                // UTC stated rather than left off: BSON's date is an absolute
                // instant with no zone of its own, and a timestamp column with
                // no timezone means local time to every consumer of Arrow.
                let mut b = TimestampMillisecondBuilder::with_capacity(rows).with_timezone("UTC");
                for v in values {
                    match v {
                        Some(Bson::DateTime(x)) => b.append_value(x.timestamp_millis()),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            ColumnType::Binary => {
                let mut b = BinaryBuilder::new();
                for v in values {
                    match v {
                        Some(Bson::Binary(x)) => b.append_value(&x.bytes),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            ColumnType::Text => {
                let mut b = StringBuilder::new();
                for v in values {
                    match v {
                        None | Some(Bson::Null) | Some(Bson::Undefined) => b.append_null(),
                        Some(other) => b.append_value(text_of(other)),
                    }
                }
                Arc::new(b.finish())
            }
        }
    }

    /// The fields of each document that the schema has no column for, as one
    /// JSON object per row — empty rather than `{}` where there are none, so an
    /// ordinary row's cell is blank instead of showing punctuation.
    fn leftovers(&self, documents: &[Document]) -> ArrayRef {
        let mut b = StringBuilder::new();
        for document in documents {
            let left: serde_json::Map<String, serde_json::Value> = document
                .iter()
                .filter(|(k, _)| !self.names.iter().any(|n| n == *k))
                .map(|(k, v)| (k.clone(), plain_json(v)))
                .collect();
            if left.is_empty() {
                b.append_null();
            } else {
                b.append_value(
                    serde_json::to_string(&serde_json::Value::Object(left))
                        .unwrap_or_else(|_| "{}".to_string()),
                );
            }
        }
        Arc::new(b.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, Int64Array, StringArray};
    use bson::{doc, oid::ObjectId};

    fn shape_of(documents: &[Document]) -> Shape {
        Shape::infer(documents)
    }

    #[test]
    fn a_uniform_collection_gets_exactly_its_own_columns() {
        let docs = vec![
            doc! { "_id": 1i32, "name": "a" },
            doc! { "_id": 2i32, "name": "b" },
        ];
        let shape = shape_of(&docs);
        let names: Vec<String> = shape.columns().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["_id", "name"]);
    }

    #[test]
    fn columns_keep_the_order_they_were_first_seen_in() {
        // A grid whose columns reshuffle between two runs of the same query is
        // unusable, and BSON preserves key order, so the order has to come from
        // somewhere deterministic rather than from a hash map.
        let docs = vec![
            doc! { "b": 1i32, "a": 2i32 },
            doc! { "a": 3i32, "c": 4i32, "b": 5i32 },
        ];
        let names: Vec<String> = shape_of(&docs)
            .columns()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names[..3], ["b", "a", "c"]);
    }

    #[test]
    fn a_ragged_collection_gets_somewhere_to_put_what_does_not_fit() {
        let docs = vec![doc! { "a": 1i32 }, doc! { "a": 2i32, "b": 3i32 }];
        let shape = shape_of(&docs);
        assert!(
            shape.columns().iter().any(|(n, _)| n == EXTRA),
            "documents that disagree should get the overflow column"
        );
    }

    #[test]
    fn a_uniform_collection_is_not_given_an_empty_column_to_look_at() {
        let docs = vec![doc! { "a": 1i32 }, doc! { "a": 2i32 }];
        assert!(!shape_of(&docs).columns().iter().any(|(n, _)| n == EXTRA));
    }

    #[test]
    fn a_field_the_sample_never_saw_is_kept_rather_than_dropped() {
        // The case this file exists for. `c` is nowhere in the sample, so no
        // column can have been made for it, and a driver that simply left it out
        // would lose data with nothing on screen to say so.
        let sample = vec![doc! { "a": 1i32 }, doc! { "a": 2i32, "b": 3i32 }];
        let shape = shape_of(&sample);
        let batch = shape
            .batch(&[doc! { "a": 9i32, "c": "surprise" }])
            .expect("batch");
        let extra = batch
            .column_by_name(EXTRA)
            .expect("the overflow column")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text");
        assert_eq!(extra.value(0), r#"{"c":"surprise"}"#);
    }

    #[test]
    fn an_ordinary_row_leaves_the_overflow_column_blank() {
        let sample = vec![doc! { "a": 1i32 }, doc! { "a": 2i32, "b": 3i32 }];
        let batch = shape_of(&sample)
            .batch(&[doc! { "a": 9i32, "b": 8i32 }])
            .expect("batch");
        let extra = batch.column_by_name(EXTRA).expect("the overflow column");
        assert!(extra.is_null(0), "nothing left over means an empty cell");
    }

    #[test]
    fn two_widths_of_integer_in_one_field_become_the_wider_one() {
        let docs = vec![doc! { "n": 1i32 }, doc! { "n": 5_000_000_000i64 }];
        let shape = shape_of(&docs);
        assert_eq!(shape.columns()[0].1, ColumnType::Int64);
        let batch = shape.batch(&docs).expect("batch");
        let n = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_eq!(n.value(0), 1);
        assert_eq!(n.value(1), 5_000_000_000);
    }

    #[test]
    fn a_large_integer_beside_a_double_is_not_quietly_rounded() {
        // The reason `unify` widens `Int32` into `Float64` but not `Int64`: a
        // double cannot hold every 64-bit integer, and the ones it cannot are
        // exactly the identifiers and amounts somebody would notice. Text keeps
        // the digits that were stored.
        let docs = vec![
            doc! { "amount": 9_007_199_254_740_993i64 },
            doc! { "amount": 1.5f64 },
        ];
        let shape = shape_of(&docs);
        assert_eq!(shape.columns()[0].1, ColumnType::Text);
        let batch = shape.batch(&docs).expect("batch");
        let amount = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text");
        assert_eq!(amount.value(0), "9007199254740993");
    }

    #[test]
    fn a_field_that_is_null_in_one_document_does_not_lose_its_type() {
        let docs = vec![doc! { "n": Bson::Null }, doc! { "n": 7i32 }];
        assert_eq!(shape_of(&docs).columns()[0].1, ColumnType::Int32);
    }

    #[test]
    fn a_missing_field_and_a_null_one_both_read_as_empty() {
        let sample = vec![doc! { "a": 1i32, "b": 2i32 }];
        let batch = shape_of(&sample)
            .batch(&[doc! { "a": 1i32, "b": Bson::Null }, doc! { "a": 2i32 }])
            .expect("batch");
        let b = batch.column_by_name("b").expect("b");
        assert!(b.is_null(0) && b.is_null(1));
    }

    #[test]
    fn an_object_id_reads_as_the_digits_everyone_writes_it_with() {
        // Extended JSON would render this as {"$oid": "..."}, which is the form
        // a parser needs and not the one a person copies out of a cell.
        let id = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").expect("a valid id");
        let docs = vec![doc! { "_id": id }];
        let batch = shape_of(&docs).batch(&docs).expect("batch");
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text");
        assert_eq!(ids.value(0), "65a1b2c3d4e5f60718293a4b");
    }

    #[test]
    fn a_nested_document_is_one_cell_of_readable_json() {
        // Not flattened into `address.city`, which reads well until a document
        // without an address turns the grid into mostly-null columns.
        let docs = vec![doc! { "address": doc! { "city": "Taipei", "zip": 100i32 } }];
        let batch = shape_of(&docs).batch(&docs).expect("batch");
        let cell = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text");
        assert_eq!(cell.value(0), r#"{"city":"Taipei","zip":100}"#);
    }

    #[test]
    fn an_empty_result_states_no_columns_rather_than_guessing_at_id() {
        assert!(shape_of(&[]).columns().is_empty());
    }

    #[test]
    fn a_document_of_the_same_fields_in_another_order_is_not_a_second_shape() {
        // BSON preserves key order, so two writers of the same struct can
        // produce different orders. Treating that as disagreement would put an
        // overflow column on a collection that is perfectly uniform.
        let docs = vec![doc! { "a": 1i32, "b": 2i32 }, doc! { "b": 3i32, "a": 4i32 }];
        assert!(!shape_of(&docs).columns().iter().any(|(n, _)| n == EXTRA));
    }
}
