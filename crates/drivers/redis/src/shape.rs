//! Giving a Redis reply a set of columns.
//!
//! Two shapes live here, and they exist for different reasons.
//!
//! **A reply, as columns.** Redis answers are positional: a bulk string, an
//! integer, an array, a map. Nothing in the protocol names a column, so the names
//! have to be invented, and `Reply` invents as few as it can — one column for a
//! reply that is one thing, two for a reply that is pairs. What each is called is
//! argued at `VALUE` and `FIELD`.
//!
//! **A page of keys, as rows.** This is the shape a browse produces, and the one
//! that makes the `Driver` trait fit a key-value store at all: the rows of the
//! `hash` relation are the keys that hold hashes. The column set is fixed per
//! type, `key_columns` is the single place it is written down, and `metadata.rs`
//! reads the same function — so the structure pane cannot describe a relation the
//! grid does not produce.
//!
//! The values are read whole. A `hash` cell holds the entire hash as a JSON
//! object, a `zset` its members with their scores, a `stream` its entries. That
//! is what "type-aware value display" means here, and its cost is stated rather
//! than avoided: browsing a hundred keys that each hold a million-element list
//! reads a hundred million elements. The alternative — reading a bounded prefix —
//! was rejected because a cell showing the first ten members of a set, with
//! nothing on screen to say it is the first ten, is the silent kind of wrong. The
//! `size` column is there so the reader can see what a cell will cost before
//! opening it.

use arrow::array::{
    ArrayRef, BooleanBuilder, Float64Builder, Int64Builder, RecordBatch, StringBuilder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::error::ArrowError;
use redis::{Cmd, Value};
use std::collections::VecDeque;
use std::sync::Arc;

/// What a reply's one column is called when the reply does not name it.
///
/// Redis says what a thing is and never what it is called: `GET k` answers with
/// bytes, `LRANGE k 0 -1` with an array of bytes. Two other names were
/// considered. The command — a column headed `GET` — names the question rather
/// than the answer, and stops meaning anything the moment two commands are on one
/// statement. `reply` describes the protocol, which is this client explaining its
/// own plumbing to somebody who asked about their data. `value` is what the cell
/// holds, and it is also the word Redis's own documentation uses for the half of
/// a key that is not the key.
pub const VALUE: &str = "value";

/// The left column of a map reply, in Redis's own word for it.
///
/// `HGETALL` returns a hash's fields, and `CONFIG GET` its parameters; `field` is
/// right for the first and near enough for the second. `key` was rejected because
/// it is already taken, by the thing a browse lists — a `key` column holding
/// hash field names beside a `key` column holding key names is two different
/// meanings under one heading.
pub const FIELD: &str = "field";

/// The six types a Redis value can have, which are this driver's relations.
///
/// A closed set, and deliberately not the open one `TYPE` can return. A module
/// can add its own — `ReJSON-RL`, `TSDB-TYPE` — and there is no command that
/// reads one generically, so a relation for it would be a relation whose values
/// cannot be shown. Those keys are still visible: a `SCAN` that names no type
/// lists them with the server's own word in the `type` column and an empty value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
}

/// Every type, in the order the navigator lists them: the scalar first, then the
/// collections in the order Redis's own documentation introduces them.
pub const TYPES: [KeyType; 6] = [
    KeyType::String,
    KeyType::Hash,
    KeyType::List,
    KeyType::Set,
    KeyType::ZSet,
    KeyType::Stream,
];

impl KeyType {
    /// The name `TYPE` answers with, which is also the relation's name and the
    /// word `SCAN ... TYPE` expects. One spelling for all three, because they are
    /// one thing.
    pub fn name(self) -> &'static str {
        match self {
            KeyType::String => "string",
            KeyType::Hash => "hash",
            KeyType::List => "list",
            KeyType::Set => "set",
            KeyType::ZSet => "zset",
            KeyType::Stream => "stream",
        }
    }

    pub fn parse(name: &str) -> Option<KeyType> {
        TYPES.into_iter().find(|t| t.name() == name)
    }

    /// Whether this type holds more than one thing, which is what decides
    /// whether the relation has a `size` column.
    pub fn is_collection(self) -> bool {
        self != KeyType::String
    }

    /// The command that reads the whole value.
    ///
    /// Whole, in every case. `LRANGE k 0 -1` and `XRANGE k - +` are the
    /// unbounded forms on purpose — see the module doc for what that costs and
    /// why a bounded one is worse.
    pub fn read(self, key: &[u8]) -> Cmd {
        let mut cmd = redis::cmd(match self {
            KeyType::String => "GET",
            KeyType::Hash => "HGETALL",
            KeyType::List => "LRANGE",
            KeyType::Set => "SMEMBERS",
            KeyType::ZSet => "ZRANGE",
            KeyType::Stream => "XRANGE",
        });
        cmd.arg(key);
        match self {
            KeyType::List => {
                cmd.arg(0).arg(-1);
            }
            KeyType::ZSet => {
                // WITHSCORES, because a sorted set without its scores is a list
                // in an order nobody can account for.
                cmd.arg(0).arg(-1).arg("WITHSCORES");
            }
            KeyType::Stream => {
                cmd.arg("-").arg("+");
            }
            _ => {}
        }
        cmd
    }

    /// The command that counts the value, for the types that have a count.
    pub fn size(self, key: &[u8]) -> Option<Cmd> {
        let name = match self {
            KeyType::String => return None,
            KeyType::Hash => "HLEN",
            KeyType::List => "LLEN",
            KeyType::Set => "SCARD",
            KeyType::ZSet => "ZCARD",
            KeyType::Stream => "XLEN",
        };
        let mut cmd = redis::cmd(name);
        cmd.arg(key);
        Some(cmd)
    }

    /// This type's value as one cell of text.
    ///
    /// A string is its own bytes and nothing else — quoting it as JSON would put
    /// quotation marks around every value in the commonest relation there is.
    /// Everything else is JSON, because a collection in a grid cell has to be one
    /// piece of text and JSON is the one a reader already knows how to read.
    pub fn render(self, value: &Value) -> Option<String> {
        if matches!(value, Value::Nil) {
            return None;
        }
        Some(match self {
            KeyType::String => text(value),
            // A sorted set is its ranking, so the members come back in rank order
            // as objects rather than as a `{member: score}` map: JSON objects are
            // unordered by definition, and the order is the whole point of the
            // type.
            KeyType::ZSet => render_json(&scored(value)),
            KeyType::Stream => render_json(&entries(value)),
            _ => render_json(&json(value)),
        })
    }
}

/// One column of a listing of keys.
///
/// Carries the declared type as well as the Arrow one because the structure pane
/// shows what the database says a column is, and Redis says nothing at all — so
/// the words here are invented, and inventing them once is the point of this
/// struct existing.
pub struct KeyColumn {
    pub name: &'static str,
    pub arrow: DataType,
    /// What to show in the structure pane's type column.
    pub declared: &'static str,
    pub nullable: bool,
}

/// The columns a listing of keys of `of` has, or of mixed types when `of` is
/// `None`.
///
/// The single place the browse's shape is written down. `metadata::columns` reads
/// this and so does the row builder, so a structure pane cannot promise a column
/// the grid does not produce.
pub fn key_columns(of: Option<KeyType>) -> Vec<KeyColumn> {
    let mut columns = vec![
        KeyColumn {
            name: "key",
            arrow: DataType::Utf8,
            declared: "key",
            // The one column that is never empty. A row exists because `SCAN`
            // named a key, so the key is the one thing that is certainly there.
            nullable: false,
        },
        KeyColumn {
            name: "ttl",
            arrow: DataType::Int64,
            declared: "seconds",
            // Empty for a key with no expiry, which is most keys. See `KeyRow`
            // for why the two ways of having no TTL both read as empty.
            nullable: true,
        },
    ];
    if of.is_none() {
        columns.push(KeyColumn {
            name: "type",
            arrow: DataType::Utf8,
            declared: "type",
            nullable: true,
        });
    }
    if of.is_none_or(KeyType::is_collection) {
        columns.push(KeyColumn {
            name: "size",
            arrow: DataType::Int64,
            declared: "count",
            nullable: true,
        });
    }
    columns.push(KeyColumn {
        name: VALUE,
        arrow: DataType::Utf8,
        declared: of.map_or(VALUE, KeyType::name),
        // A key `SCAN` saw can be gone by the time its value is read, which is
        // ordinary in a keyspace somebody is writing to.
        nullable: true,
    });
    columns
}

fn key_schema(of: Option<KeyType>) -> SchemaRef {
    Arc::new(Schema::new(
        key_columns(of)
            .into_iter()
            .map(|c| Field::new(c.name, c.arrow, c.nullable))
            .collect::<Vec<_>>(),
    ))
}

/// One key, read.
#[derive(Debug, Clone)]
pub struct KeyRow {
    pub key: String,
    /// Seconds until expiry, or `None` for a key that has no expiry set.
    ///
    /// `TTL` answers -1 for "no expiry" and -2 for "no such key", and both become
    /// empty here. They are genuinely different facts, and the second one is
    /// unactionable: it means the key was there when `SCAN` listed it and gone
    /// when this read it, in which case `value` is empty too and the row is its
    /// own explanation.
    pub ttl: Option<i64>,
    /// The server's own word for the type, carried only in the mixed listing.
    pub kind: Option<String>,
    pub size: Option<i64>,
    pub value: Option<String>,
}

/// A page of keys, as one batch.
pub fn key_batch(of: Option<KeyType>, rows: &[KeyRow]) -> Result<RecordBatch, ArrowError> {
    let schema = key_schema(of);
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());

    let mut keys = StringBuilder::new();
    for row in rows {
        keys.append_value(&row.key);
    }
    columns.push(Arc::new(keys.finish()));

    let mut ttls = Int64Builder::with_capacity(rows.len());
    for row in rows {
        match row.ttl {
            Some(seconds) => ttls.append_value(seconds),
            None => ttls.append_null(),
        }
    }
    columns.push(Arc::new(ttls.finish()));

    if of.is_none() {
        let mut kinds = StringBuilder::new();
        for row in rows {
            match &row.kind {
                Some(kind) => kinds.append_value(kind),
                None => kinds.append_null(),
            }
        }
        columns.push(Arc::new(kinds.finish()));
    }

    if of.is_none_or(KeyType::is_collection) {
        let mut sizes = Int64Builder::with_capacity(rows.len());
        for row in rows {
            match row.size {
                Some(n) => sizes.append_value(n),
                None => sizes.append_null(),
            }
        }
        columns.push(Arc::new(sizes.finish()));
    }

    let mut values = StringBuilder::new();
    for row in rows {
        match &row.value {
            Some(value) => values.append_value(value),
            None => values.append_null(),
        }
    }
    columns.push(Arc::new(values.finish()));

    RecordBatch::try_new(schema, columns)
}

/// The schema a listing of keys has, before any key has been read.
pub fn key_shape(of: Option<KeyType>) -> SchemaRef {
    key_schema(of)
}

/// A reply, arranged into columns.
pub struct Reply {
    schema: SchemaRef,
    rows: Rows,
}

enum Rows {
    /// One row of one column: everything that is not a container.
    One(Value),
    /// One row per element, for an array or a set.
    Many(Vec<Value>),
    /// Two columns, for RESP3's map reply.
    Pairs(Vec<(Value, Value)>),
}

impl Reply {
    pub fn of(value: Value) -> Reply {
        // An attribute is metadata the server volunteered about the reply — a
        // cache hint, a popularity count — wrapped around the reply itself.
        // Unwrapped rather than shown, because a grid that displayed it would be
        // showing an answer to a question nobody asked, in place of the one they
        // did.
        let value = match value {
            Value::Attribute { data, .. } => *data,
            other => other,
        };
        let rows = match value {
            Value::Array(items) | Value::Set(items) => Rows::Many(items),
            Value::Map(pairs) => Rows::Pairs(pairs),
            // A push is a message the server sent unasked, which this driver
            // never subscribes for. Shown as the list it is rather than
            // discarded, since arriving at all is the surprising part.
            Value::Push { data, .. } => Rows::Many(data),
            other => Rows::One(other),
        };
        let schema = match &rows {
            Rows::One(value) => Arc::new(Schema::new(vec![Field::new(
                VALUE,
                scalar_type(value),
                true,
            )])),
            // Text, whatever the elements are. An array is free to mix an
            // integer, a string and a nested array in one reply — `CONFIG GET`
            // and `XRANGE` both do — so a column typed from the first element
            // would be a column the second one does not fit.
            Rows::Many(_) => Arc::new(Schema::new(vec![Field::new(VALUE, DataType::Utf8, true)])),
            Rows::Pairs(_) => Arc::new(Schema::new(vec![
                Field::new(FIELD, DataType::Utf8, true),
                Field::new(VALUE, DataType::Utf8, true),
            ])),
        };
        Reply { schema, rows }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// This reply as batches of at most `batch_rows` rows.
    ///
    /// Built up front rather than lazily, because a Redis reply is already
    /// entirely in memory by the time it gets here: the protocol has no
    /// streaming form, and `query_async` returns a whole `Value`. Pretending to
    /// stream it would be this driver adding a fiction the database does not
    /// have.
    pub fn into_batches(self, batch_rows: usize) -> Result<VecDeque<RecordBatch>, ArrowError> {
        let batch_rows = batch_rows.max(1);
        let mut out = VecDeque::new();
        match self.rows {
            Rows::One(value) => {
                out.push_back(RecordBatch::try_new(
                    Arc::clone(&self.schema),
                    vec![scalar_column(&value)],
                )?);
            }
            Rows::Many(items) => {
                for page in items.chunks(batch_rows) {
                    let mut b = StringBuilder::new();
                    for item in page {
                        match item {
                            Value::Nil => b.append_null(),
                            other => b.append_value(text(other)),
                        }
                    }
                    out.push_back(RecordBatch::try_new(
                        Arc::clone(&self.schema),
                        vec![Arc::new(b.finish()) as ArrayRef],
                    )?);
                }
            }
            Rows::Pairs(pairs) => {
                for page in pairs.chunks(batch_rows) {
                    let mut fields = StringBuilder::new();
                    let mut values = StringBuilder::new();
                    for (field, value) in page {
                        fields.append_value(text(field));
                        match value {
                            Value::Nil => values.append_null(),
                            other => values.append_value(text(other)),
                        }
                    }
                    out.push_back(RecordBatch::try_new(
                        Arc::clone(&self.schema),
                        vec![
                            Arc::new(fields.finish()) as ArrayRef,
                            Arc::new(values.finish()) as ArrayRef,
                        ],
                    )?);
                }
            }
        }
        Ok(out)
    }
}

/// What Arrow type a single value is shown as.
///
/// Only where the protocol is certain. RESP3 states that an integer is an
/// integer and a double a double, so those keep their types and sort and align
/// as numbers in a grid. Everything else is text — including a bulk string that
/// happens to hold digits, since Redis stores numbers as strings and guessing
/// which ones are meant to be numeric would turn a version string into a float.
fn scalar_type(value: &Value) -> DataType {
    match value {
        Value::Int(_) => DataType::Int64,
        Value::Double(_) => DataType::Float64,
        Value::Boolean(_) => DataType::Boolean,
        _ => DataType::Utf8,
    }
}

fn scalar_column(value: &Value) -> ArrayRef {
    match value {
        Value::Int(n) => {
            let mut b = Int64Builder::with_capacity(1);
            b.append_value(*n);
            Arc::new(b.finish())
        }
        Value::Double(f) => {
            let mut b = Float64Builder::with_capacity(1);
            b.append_value(*f);
            Arc::new(b.finish())
        }
        Value::Boolean(v) => {
            let mut b = BooleanBuilder::with_capacity(1);
            b.append_value(*v);
            Arc::new(b.finish())
        }
        Value::Nil => {
            let mut b = StringBuilder::new();
            b.append_null();
            Arc::new(b.finish())
        }
        other => {
            let mut b = StringBuilder::new();
            b.append_value(text(other));
            Arc::new(b.finish())
        }
    }
}

/// A value as one cell of text.
///
/// A scalar is its own characters, so that a cell holding a version number shows
/// `7.4.10` and not `"7.4.10"`. A container is JSON, because a cell is one piece
/// of text and there is no other rendering of a nested array that a reader can
/// take apart again.
pub fn text(value: &Value) -> String {
    match value {
        Value::Nil => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Boolean(v) => v.to_string(),
        // Lossily, and it is the only choice: a Redis string is arbitrary bytes,
        // an Arrow `Utf8` column is not, and a grid cannot show a byte that is
        // not a character. A user who stored a JPEG under a key sees replacement
        // characters, which at least says something is there.
        Value::BulkString(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::SimpleString(s) => s.clone(),
        Value::Okay => "OK".to_string(),
        Value::VerbatimString { text, .. } => text.clone(),
        _ => render_json(&json(value)),
    }
}

fn render_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_string())
}

/// A reply as the JSON a person would write.
fn json(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Nil => J::Null,
        Value::Int(n) => J::from(*n),
        Value::Double(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            // The infinities are values RESP3 carries and JSON has no spelling
            // for. Their names are better than a null claiming there was nothing.
            .unwrap_or_else(|| J::String(f.to_string())),
        Value::Boolean(v) => J::Bool(*v),
        Value::BulkString(bytes) => J::String(String::from_utf8_lossy(bytes).into_owned()),
        Value::SimpleString(s) => J::String(s.clone()),
        Value::Okay => J::String("OK".to_string()),
        Value::VerbatimString { text, .. } => J::String(text.clone()),
        Value::Array(items) | Value::Set(items) => J::Array(items.iter().map(json).collect()),
        Value::Map(pairs) => J::Object(
            pairs
                .iter()
                .map(|(field, value)| (text(field), json(value)))
                .collect(),
        ),
        Value::Attribute { data, .. } => json(data),
        Value::Push { data, .. } => J::Array(data.iter().map(json).collect()),
        // `Value` is `#[non_exhaustive]`, and this arm is what that costs: a
        // big number, a server error nested inside an array, or a variant a
        // later redis-rs adds. Debug is not a rendering anybody wants in a cell,
        // and it is better than dropping the value — the same trade the MongoDB
        // driver makes in `_extra`.
        other => J::String(format!("{other:?}")),
    }
}

/// A sorted set's reply as members with their scores, in rank order.
///
/// RESP3 answers `ZRANGE ... WITHSCORES` with an array of `[member, score]`
/// pairs, where the score really is a double — under RESP2 it is a flat array of
/// alternating strings, which is one of the reasons this driver asks for RESP3.
fn scored(value: &Value) -> serde_json::Value {
    let Value::Array(items) = value else {
        return json(value);
    };
    serde_json::Value::Array(
        items
            .iter()
            .map(|pair| match pair {
                Value::Array(both) if both.len() == 2 => serde_json::json!({
                    "member": text(&both[0]),
                    "score": json(&both[1]),
                }),
                other => json(other),
            })
            .collect(),
    )
}

/// A stream's reply as its entries.
///
/// `XRANGE` answers with an array of `[id, [field, value, field, value, …]]`.
/// The fields arrive flat even under RESP3 — a stream entry is not a map on the
/// wire — so they are paired up here into the object they describe.
fn entries(value: &Value) -> serde_json::Value {
    let Value::Array(items) = value else {
        return json(value);
    };
    serde_json::Value::Array(
        items
            .iter()
            .map(|entry| match entry {
                Value::Array(parts) if parts.len() == 2 => {
                    let fields = match &parts[1] {
                        Value::Array(flat) => flat
                            .chunks(2)
                            .map(|pair| match pair {
                                [field, value] => (text(field), json(value)),
                                // An odd number of elements is the server
                                // contradicting its own format. Kept under its
                                // own name rather than dropped.
                                [field] => (text(field), serde_json::Value::Null),
                                _ => (String::new(), serde_json::Value::Null),
                            })
                            .collect(),
                        other => {
                            let mut one = serde_json::Map::new();
                            one.insert("fields".to_string(), json(other));
                            one
                        }
                    };
                    serde_json::json!({ "fields": fields, "id": text(&parts[0]) })
                }
                other => json(other),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, Int64Array, StringArray};

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    fn column<'a>(batch: &'a RecordBatch, name: &str) -> &'a StringArray {
        batch
            .column_by_name(name)
            .expect("the column should be there")
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text")
    }

    #[test]
    fn a_string_relation_has_no_size_and_a_collection_does() {
        let names: Vec<&str> = key_columns(Some(KeyType::String))
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["key", "ttl", "value"]);
        let names: Vec<&str> = key_columns(Some(KeyType::Hash))
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["key", "ttl", "size", "value"]);
    }

    /// The mixed listing is the only one that has to say what each row is,
    /// because the rows disagree.
    #[test]
    fn a_listing_of_no_particular_type_carries_the_type_of_each_row() {
        let names: Vec<&str> = key_columns(None).iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["key", "ttl", "type", "size", "value"]);
    }

    /// The structure pane and the grid read the same function, so they cannot
    /// drift. This is that claim, written down.
    #[test]
    fn the_columns_a_relation_declares_are_the_ones_its_rows_arrive_in() {
        for of in TYPES {
            let declared: Vec<&str> = key_columns(Some(of)).iter().map(|c| c.name).collect();
            let produced: Vec<String> = key_shape(Some(of))
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
            assert_eq!(declared, produced, "{}", of.name());
        }
    }

    #[test]
    fn a_key_with_no_expiry_shows_an_empty_ttl_rather_than_minus_one() {
        // -1 is Redis's way of saying "no expiry" and it is not a duration. A
        // grid showing -1 seconds would be showing the protocol's sentinel as
        // though it were the user's data.
        let rows = vec![KeyRow {
            key: "greeting".to_string(),
            ttl: None,
            kind: None,
            size: None,
            value: Some("hello".to_string()),
        }];
        let batch = key_batch(Some(KeyType::String), &rows).expect("batch");
        assert!(batch.column_by_name("ttl").expect("ttl").is_null(0));
        assert_eq!(column(&batch, "value").value(0), "hello");
    }

    #[test]
    fn a_hash_arrives_as_a_json_object() {
        let value = Value::Map(vec![
            (bulk("born"), bulk("1815")),
            (bulk("name"), bulk("ada")),
        ]);
        assert_eq!(
            KeyType::Hash.render(&value).expect("a value"),
            r#"{"born":"1815","name":"ada"}"#
        );
    }

    #[test]
    fn a_string_is_its_own_characters_and_not_a_quoted_json_string() {
        // The commonest relation there is. Quoting would put a pair of quotation
        // marks around every cell in it.
        assert_eq!(
            KeyType::String.render(&bulk("7.4.10")).expect("a value"),
            "7.4.10"
        );
    }

    #[test]
    fn a_sorted_set_keeps_its_rank_order_and_its_scores() {
        // A JSON object would lose the order, which is the only reason the type
        // exists.
        let value = Value::Array(vec![
            Value::Array(vec![bulk("alpha"), Value::Double(1.0)]),
            Value::Array(vec![bulk("beta"), Value::Double(2.5)]),
        ]);
        assert_eq!(
            KeyType::ZSet.render(&value).expect("a value"),
            r#"[{"member":"alpha","score":1.0},{"member":"beta","score":2.5}]"#
        );
    }

    #[test]
    fn a_stream_entry_pairs_up_the_fields_the_wire_sends_flat() {
        let value = Value::Array(vec![Value::Array(vec![
            bulk("1700000000000-0"),
            Value::Array(vec![bulk("temp"), bulk("21"), bulk("unit"), bulk("c")]),
        ])]);
        assert_eq!(
            KeyType::Stream.render(&value).expect("a value"),
            r#"[{"fields":{"temp":"21","unit":"c"},"id":"1700000000000-0"}]"#
        );
    }

    #[test]
    fn a_key_that_vanished_between_the_scan_and_the_read_is_an_empty_cell() {
        assert_eq!(KeyType::String.render(&Value::Nil), None);
    }

    #[test]
    fn an_integer_reply_is_one_row_of_one_integer_column() {
        let reply = Reply::of(Value::Int(42));
        assert_eq!(reply.schema().field(0).name(), VALUE);
        assert_eq!(reply.schema().field(0).data_type(), &DataType::Int64);
        let batches = reply.into_batches(10).expect("batches");
        assert_eq!(batches.len(), 1);
        let n = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_eq!(n.value(0), 42);
    }

    #[test]
    fn an_array_reply_is_one_column_and_one_row_per_element() {
        let reply = Reply::of(Value::Array(vec![bulk("a"), bulk("b"), bulk("c")]));
        assert_eq!(reply.schema().fields().len(), 1);
        let batches = reply.into_batches(2).expect("batches");
        assert_eq!(batches.len(), 2, "three rows in pages of two");
        assert_eq!(batches[0].num_rows(), 2);
        assert_eq!(batches[1].num_rows(), 1);
    }

    #[test]
    fn a_map_reply_is_two_columns() {
        // What RESP3 buys: under RESP2 this arrives as a flat array and a client
        // has to know, per command, that the elements come in pairs.
        let reply = Reply::of(Value::Map(vec![(bulk("databases"), bulk("16"))]));
        let names: Vec<String> = reply
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        assert_eq!(names, vec![FIELD, VALUE]);
        let batches = reply.into_batches(10).expect("batches");
        assert_eq!(column(&batches[0], FIELD).value(0), "databases");
        assert_eq!(column(&batches[0], VALUE).value(0), "16");
    }

    #[test]
    fn a_nil_reply_is_one_empty_cell_rather_than_no_rows() {
        // `GET missing` answered, and what it said was "nothing". No rows at all
        // is what an empty array means, and the two are different answers.
        let batches = Reply::of(Value::Nil).into_batches(10).expect("batches");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);
        assert!(batches[0].column(0).is_null(0));
    }

    #[test]
    fn an_empty_array_reply_is_no_rows_at_all() {
        let batches = Reply::of(Value::Array(vec![]))
            .into_batches(10)
            .expect("batches");
        assert!(batches.is_empty());
    }

    #[test]
    fn a_nested_array_element_becomes_json_in_its_cell() {
        let reply = Reply::of(Value::Array(vec![Value::Array(vec![
            bulk("inner"),
            Value::Int(1),
        ])]));
        let batches = reply.into_batches(10).expect("batches");
        assert_eq!(column(&batches[0], VALUE).value(0), r#"["inner",1]"#);
    }

    #[test]
    fn an_attribute_is_unwrapped_to_the_reply_it_decorates() {
        let reply = Reply::of(Value::Attribute {
            data: Box::new(Value::Int(7)),
            attributes: vec![(bulk("ttl"), Value::Int(60))],
        });
        let batches = reply.into_batches(10).expect("batches");
        let n = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_eq!(n.value(0), 7);
    }

    #[test]
    fn the_six_type_names_round_trip_through_the_word_redis_uses() {
        for of in TYPES {
            assert_eq!(KeyType::parse(of.name()), Some(of));
        }
        assert_eq!(KeyType::parse("no_such_relation_anywhere"), None);
        // A module's type is a real answer from `TYPE` and still not one of the
        // six, which is exactly the case the mixed listing exists for.
        assert_eq!(KeyType::parse("ReJSON-RL"), None);
    }
}
