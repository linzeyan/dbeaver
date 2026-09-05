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
//! nullable column is for. Everything else — a field the sample never saw, and a
//! value whose type its column cannot hold — goes into a trailing `_extra` column
//! as JSON text.
//!
//! **Why `_extra` and not simply dropping it.** Dropping is silent data loss in a
//! tool whose entire job is showing what is in the database, and it is the kind
//! that is never noticed: the row is there, the value is not, and nothing says
//! so. Failing the batch instead would make a ragged collection unreadable.
//!
//! **Why it is unconditional.** The first version of this added `_extra` only
//! when the sample found documents that disagreed, on the grounds that a column
//! empty in every row is clutter and most collections are uniform because an
//! application wrote them. That reasoning covered the wrong case. A sample can be
//! perfectly uniform and document 1001 can still hold a string where the first
//! thousand held integers — and with no `_extra` column there is nowhere for that
//! value to go, so it becomes a null cell and the guarantee above is quietly
//! false. Type disagreement is not decidable from a prefix, so the escape hatch
//! cannot be either. It is always there, and a front end that would rather not
//! show a column null in every row it has can decline to draw it — which is a
//! question about one screen, where this is a question about whether the client
//! loses data.
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
use dbconn::{SHAPE_JSON, VALUE_SHAPE};
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
    /// A field whose values are nested — a document or an array — as the JSON a
    /// person would write.
    ///
    /// Utf8 like `Text`, and a separate type for one reason: so that something
    /// downstream can tell a document from a string that happens to look like
    /// one. `metadata::columns` reports this name, and the value viewer
    /// re-indents a column that declares it rather than sniffing the value —
    /// which would eventually meet a `Text` column holding `{}`.
    ///
    /// A field holding a document in one record and a string in another unifies
    /// to `Text`, where no such promise is made.
    ///
    /// The result's own field carries `VALUE_SHAPE` as well, which is what a
    /// statement in the Query tab is left with: its columns come from no
    /// relation, so there is no declared type there to read the name off.
    Document,
    /// MongoDB's own identifier, as the 24 hex digits it is written with.
    ///
    /// Utf8 like `Text`, and separate for the same reason `Document` is: the
    /// digits alone cannot say whether the field holds an id or a string of
    /// digits, and something downstream has to know. Writing a row back is that
    /// something — an update naming a document has to send `{"$oid": …}` where
    /// the collection holds an id and a bare string where it holds a string, and
    /// the two match different documents. Guessing from the text would mean
    /// deciding a 24-character string is an id, which for a column of hashes is
    /// wrong every time.
    ///
    /// A field holding an id in one document and something else in another
    /// unifies to `Text`, where no such promise is made.
    ObjectId,
    /// The catch-all, and the honest one. A `Decimal128` is its digits, a field
    /// that held two irreconcilable types is whichever text each value renders
    /// as — see `type_of` for why each ended up here.
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
            ColumnType::Document | ColumnType::ObjectId | ColumnType::Text => DataType::Utf8,
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
/// - **Decimal128** is IEEE 754-2008 decimal: 34 significant digits with an
///   exponent that moves per value. Arrow's `Decimal128` fixes precision and
///   scale for the whole column, so mapping onto it means choosing a scale on
///   the strength of a sample and silently rescaling every value that disagrees.
///   For a type whose entire purpose is money, the digits as written are the
///   only safe answer. This is the sharpest loss in this file.
/// - **Document and Array** are the top-level-only rule above, and get a type of
///   their own rather than falling into `Text`: the text is JSON, and saying so
///   is what lets the value viewer re-indent it without guessing from the string.
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
        Bson::Document(_) | Bson::Array(_) => Some(ColumnType::Document),
        Bson::ObjectId(_) => Some(ColumnType::ObjectId),
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

/// Whether `column` will actually store this value in a column of that type.
///
/// Mirrors the match arms in `column`, and has to stay in step with them: a
/// value this says fits but that `column` writes as null is a value lost
/// silently, which is the one failure this whole file exists to prevent. The
/// duplication is deliberate — the alternative is building the column and then
/// asking it what it kept, which means keeping every row's outcome for a whole
/// batch to answer a question about one cell.
fn fits(value: &Bson, ty: ColumnType) -> bool {
    match (ty, value) {
        // A null was not stored, but nothing was lost either: the cell is empty
        // and that is exactly what the document said.
        (_, Bson::Null | Bson::Undefined) => true,
        (ColumnType::Bool, Bson::Boolean(_)) => true,
        (ColumnType::Int32, Bson::Int32(_)) => true,
        (ColumnType::Int64, Bson::Int64(_) | Bson::Int32(_)) => true,
        (ColumnType::Float64, Bson::Double(_) | Bson::Int32(_)) => true,
        (ColumnType::DateTime, Bson::DateTime(_)) => true,
        (ColumnType::Binary, Bson::Binary(_)) => true,
        // Narrower than `Text` on purpose. A string in a column that declares
        // itself JSON would be a document the viewer then failed to parse, so it
        // goes to `_extra` instead — where a value that did not fit its column
        // has always gone.
        (ColumnType::Document, Bson::Document(_) | Bson::Array(_)) => true,
        // Narrower than `Text` for the same reason: a string here would be
        // indistinguishable from an id once it is 24 hex digits in a cell.
        (ColumnType::ObjectId, Bson::ObjectId(_)) => true,
        // Text takes anything, which is why `unify` falls back to it.
        (ColumnType::Text, _) => true,
        _ => false,
    }
}

/// The metadata that says a Utf8 column holds JSON.
///
/// Both columns it goes on were written by `serde_json` — `text_of` for a nested
/// field, `leftovers` for the overflow — so the claim is about what this file
/// produced rather than about what the characters look like.
fn json_shape() -> std::collections::HashMap<String, String> {
    [(VALUE_SHAPE.to_string(), SHAPE_JSON.to_string())].into()
}

/// The columns a result will have, and where each document's leftovers go.
#[derive(Debug, Clone)]
pub struct Shape {
    /// Field names in the order they were first seen, `_extra` excluded.
    names: Vec<String>,
    types: Vec<ColumnType>,
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

        for document in sample {
            for (key, value) in document {
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
        }

        // A field that was null in every document sampled has no type to report.
        // Text is the right answer: every value in the column is null, so the
        // choice costs nothing, and it is the type that can hold whatever turns
        // up in a document the sample did not reach.
        let types = types
            .into_iter()
            .map(|t| t.unwrap_or(ColumnType::Text))
            .collect();
        Shape::build(names, types)
    }

    fn build(names: Vec<String>, types: Vec<ColumnType>) -> Shape {
        let mut fields: Vec<Field> = names
            .iter()
            .zip(&types)
            // Every column is nullable without exception. A field present in
            // every sampled document may still be missing from the next one, and
            // a schema that promised otherwise would be a promise this database
            // cannot keep.
            .map(|(name, ty)| {
                let field = Field::new(name, ty.arrow(), true);
                match ty {
                    ColumnType::Document => field.with_metadata(json_shape()),
                    _ => field,
                }
            })
            .collect();
        // An empty result has no columns at all, and giving it a lone `_extra`
        // would state that a collection has a field when nothing has
        // established it has any documents.
        if !fields.is_empty() {
            // The declaration `columns()` cannot make. The structure pane lists
            // what the collection holds and `_extra` is not one of those, so a
            // front end reading types from the relation finds nothing for this
            // column and shows the JSON as the one line it arrived on. The
            // result's own field is the only place the claim belongs.
            fields.push(Field::new(EXTRA, DataType::Utf8, true).with_metadata(json_shape()));
        }
        let schema = Arc::new(Schema::new(fields));
        Shape {
            names,
            types,
            schema,
        }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// The fields that were actually found, for the structure pane.
    ///
    /// Without `_extra`, which the result schema does carry. The two differ on
    /// purpose: the grid shows a column because the result has one, but a
    /// structure pane lists what the *collection* holds, and no document in it
    /// has a field called `_extra`. Listing the escape hatch there would state
    /// that every collection in MongoDB has a field this client invented.
    pub fn columns(&self) -> Vec<(String, ColumnType)> {
        self.names
            .iter()
            .cloned()
            .zip(self.types.iter().copied())
            .collect()
    }

    /// Packs documents into one batch of this shape.
    pub fn batch(&self, documents: &[Document]) -> Result<RecordBatch, MongoError> {
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(self.names.len() + 1);
        for (at, ty) in self.types.iter().enumerate() {
            columns.push(self.column(&self.names[at], *ty, documents));
        }
        if !columns.is_empty() {
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
            ColumnType::Document => {
                let mut b = StringBuilder::new();
                for v in values {
                    match v {
                        // The pair `fits` names: anything else in this column is
                        // a null here and a JSON object in `_extra`, rather than
                        // a bare string sitting under a type that says JSON.
                        Some(nested @ (Bson::Document(_) | Bson::Array(_))) => {
                            b.append_value(text_of(nested))
                        }
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
            ColumnType::ObjectId => {
                let mut b = StringBuilder::new();
                for v in values {
                    match v {
                        // The one value `fits` names: a string that reached this
                        // column is a string, and putting its characters here
                        // would make the column's type a lie.
                        Some(id @ Bson::ObjectId(_)) => b.append_value(text_of(id)),
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

    /// Everything in each document that its own column could not take, as one
    /// JSON object per row — null rather than `{}` where there is nothing, so an
    /// ordinary row's cell is blank instead of showing punctuation.
    ///
    /// Two things end up here, and the second is the one that is easy to miss.
    /// A field the schema has no column for is obvious. A field that *has* a
    /// column, holding a value that column's type cannot represent, is not: the
    /// builder in `column` writes a null for it, and without this that null is
    /// the only trace the value ever existed.
    fn leftovers(&self, documents: &[Document]) -> ArrayRef {
        let mut b = StringBuilder::new();
        for document in documents {
            let left: serde_json::Map<String, serde_json::Value> = document
                .iter()
                .filter(|(k, v)| match self.names.iter().position(|n| n == *k) {
                    Some(at) => !fits(v, self.types[at]),
                    None => true,
                })
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

    /// The result's columns, which is `columns()` plus the escape hatch.
    fn result_columns(shape: &Shape) -> Vec<String> {
        shape
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    #[test]
    fn the_escape_hatch_is_the_last_column_and_is_always_there() {
        let docs = vec![
            doc! { "_id": 1i32, "name": "a" },
            doc! { "_id": 2i32, "name": "b" },
        ];
        let shape = shape_of(&docs);
        assert_eq!(result_columns(&shape), vec!["_id", "name", EXTRA]);
        let fields: Vec<String> = shape.columns().into_iter().map(|(n, _)| n).collect();
        assert_eq!(fields, vec!["_id", "name"], "the hatch is not a field");
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
            result_columns(&shape).iter().any(|n| n == EXTRA),
            "documents that disagree should get the overflow column"
        );
    }

    #[test]
    fn a_uniform_collection_pays_for_the_hatch_with_a_blank_column_and_nothing_else() {
        // The cost of making the escape hatch unconditional, stated as a test so
        // it is a known price rather than a surprise: a collection where nothing
        // overflows gets a column that is null in every row.
        let docs = vec![doc! { "a": 1i32 }, doc! { "a": 2i32 }];
        let batch = shape_of(&docs).batch(&docs).expect("batch");
        let extra = batch.column_by_name(EXTRA).expect("always present");
        assert!((0..batch.num_rows()).all(|r| extra.is_null(r)));
    }

    #[test]
    fn a_value_its_own_column_cannot_hold_is_kept_rather_than_nulled() {
        // The case that made the hatch unconditional. Every sampled document
        // agrees that `n` is an integer, so the column is Int32 and there is no
        // disagreement a prefix could have detected -- and then a later document
        // holds a string. Without somewhere to put it the cell is null and the
        // value is gone with nothing on screen to say so.
        let sample = vec![doc! { "n": 1i32 }, doc! { "n": 2i32 }];
        let shape = shape_of(&sample);
        assert_eq!(shape.columns()[0].1, ColumnType::Int32);

        let batch = shape.batch(&[doc! { "n": "five" }]).expect("batch");
        assert!(
            batch.column(0).is_null(0),
            "the integer column cannot take it"
        );
        let extra = batch
            .column_by_name(EXTRA)
            .expect("the overflow column")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text");
        assert_eq!(extra.value(0), r#"{"n":"five"}"#);
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
        // The column still says what those digits are, which is what an update
        // naming this document reads to decide between {"$oid": …} and the bare
        // string.
        assert_eq!(shape_of(&docs).columns()[0].1, ColumnType::ObjectId);
        // And the id is only in its own column. `_extra` holds what a column
        // could not take, so an id showing up there would mean the schema is
        // claiming a type its own builder does not honour — which is the pair
        // this file's `fits` exists to keep in step.
        let extra = batch.column_by_name(EXTRA).expect("the escape hatch");
        assert!(extra.is_null(0), "nothing was left over");
    }

    #[test]
    fn a_field_holding_an_id_and_a_string_stops_claiming_to_hold_ids() {
        let id = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").expect("a valid id");
        let docs = vec![
            doc! { "ref": id },
            doc! { "ref": "65a1b2c3d4e5f60718293a4b" },
        ];
        let shape = shape_of(&docs);
        assert_eq!(shape.columns()[0].1, ColumnType::Text);
        // Both rows still read, which is the point of falling back rather than
        // sending the string to `_extra`.
        let batch = shape.batch(&docs).expect("batch");
        let cells = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text");
        assert_eq!(cells.value(0), cells.value(1));
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
    fn a_nested_field_says_it_holds_a_document_rather_than_text() {
        // The name `metadata::columns` reports, and the only thing that tells the
        // value viewer this cell is worth re-indenting. As `Text` it stayed the
        // one long line the grid cannot show either.
        let docs = vec![doc! { "address": doc! { "city": "Taipei" } }];
        assert_eq!(shape_of(&docs).columns()[0].1, ColumnType::Document);

        // An array is the same kind of value and gets the same answer: `[1,2]`
        // is JSON, and a reader opening it wants it laid out.
        let arrays = vec![doc! { "tags": vec!["a", "b"] }];
        assert_eq!(shape_of(&arrays).columns()[0].1, ColumnType::Document);
    }

    #[test]
    fn a_field_holding_a_document_and_a_string_is_text_rather_than_either() {
        // The type's whole promise is that the cell parses as JSON, and this
        // field cannot keep it. Text promises nothing, so both values render.
        let docs = vec![
            doc! { "note": doc! { "body": "hi" } },
            doc! { "note": "hi" },
        ];
        assert_eq!(shape_of(&docs).columns()[0].1, ColumnType::Text);
    }

    #[test]
    fn a_string_arriving_in_a_document_column_goes_to_the_overflow() {
        // The 1001st document, which is the case `_extra` is unconditional for:
        // a uniform sample gives the column its type, and the string that turns
        // up afterwards still has to be somewhere the reader can see it. If
        // `fits` and the builder ever disagree about this pair, the value is a
        // null cell and nothing else.
        let sample = vec![doc! { "note": doc! { "body": "hi" } }];
        let batch = shape_of(&sample)
            .batch(&[doc! { "note": "plain text" }])
            .expect("batch");
        assert!(batch.column_by_name("note").expect("note").is_null(0));
        let extra = batch
            .column_by_name(EXTRA)
            .expect("present")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text");
        assert_eq!(extra.value(0), r#"{"note":"plain text"}"#);
    }

    #[test]
    fn an_empty_result_states_no_columns_rather_than_guessing_at_id() {
        assert!(shape_of(&[]).columns().is_empty());
    }

    /// What the field says, for the two columns whose values this file wrote as
    /// JSON.
    fn shape_of_field(shape: &Shape, name: &str) -> Option<String> {
        shape
            .schema()
            .field_with_name(name)
            .ok()?
            .metadata()
            .get(VALUE_SHAPE)
            .cloned()
    }

    #[test]
    fn the_overflow_column_says_on_the_field_that_it_holds_json() {
        // The limitation this closes. `columns()` deliberately omits `_extra` —
        // no document has a field this client invented — so a reader taking
        // types from the relation finds nothing for it and shows the one line
        // `serde_json` wrote. The result's own field is where the claim can be
        // made without stating that the collection has the column.
        let docs = vec![doc! { "a": 1i32 }, doc! { "a": 2i32, "b": 3i32 }];
        let shape = shape_of(&docs);
        assert_eq!(shape_of_field(&shape, EXTRA).as_deref(), Some(SHAPE_JSON));
        assert!(
            shape.columns().iter().all(|(n, _)| n != EXTRA),
            "and it is still not a field of the collection"
        );
    }

    #[test]
    fn a_document_column_says_it_on_the_field_as_well_as_in_its_type() {
        // The declared type is only reachable for a browsed collection. A
        // statement in the Query tab returns columns no relation describes, and
        // before this the same nested value laid out on one tab and not on the
        // other.
        let docs = vec![doc! { "address": doc! { "city": "Taipei" } }];
        let shape = shape_of(&docs);
        assert_eq!(
            shape_of_field(&shape, "address").as_deref(),
            Some(SHAPE_JSON)
        );
    }

    #[test]
    fn a_column_that_is_not_json_says_nothing_on_its_field() {
        // The negative half, and the one that matters: a rule loose enough to
        // mark the ObjectId column would hand 24 hex digits to a JSON parser
        // that fails, and the failure is drawn where the value used to be.
        let id = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").expect("a valid id");
        let docs = vec![doc! { "_id": id, "name": "a", "n": 1i32 }];
        let shape = shape_of(&docs);
        for column in ["_id", "name", "n"] {
            assert_eq!(
                shape_of_field(&shape, column),
                None,
                "{column} holds no JSON and must not claim to"
            );
        }
    }

    #[test]
    fn a_document_of_the_same_fields_in_another_order_is_not_a_second_shape() {
        // BSON preserves key order, so two writers of the same struct can
        // produce different orders. That must not be read as the documents
        // holding different fields.
        let docs = vec![doc! { "a": 1i32, "b": 2i32 }, doc! { "b": 3i32, "a": 4i32 }];
        assert_eq!(result_columns(&shape_of(&docs)), vec!["a", "b", EXTRA]);
    }

    #[test]
    fn a_null_is_not_treated_as_a_value_that_did_not_fit() {
        // A null in an integer column is an empty cell because the document said
        // so, not because anything was refused. Sending it to the overflow would
        // fill `_extra` with noise on every sparse collection.
        let sample = vec![doc! { "n": 1i32 }];
        let batch = shape_of(&sample)
            .batch(&[doc! { "n": Bson::Null }])
            .expect("batch");
        assert!(batch.column_by_name(EXTRA).expect("present").is_null(0));
    }
}
