//! A grid's staged changes, as the command documents that would apply them.
//!
//! The counterpart of `browse` in `driver.rs`: that one writes the `find` a
//! listing of documents comes from, and this one writes the `update`, `insert`
//! and `delete` that put a change back. Both are here rather than above the
//! driver because MongoDB has no SQL for the layer above to compose, and both
//! name the database with `$db` for the same reason — it is the field the wire
//! protocol carries it in, so a change reaches the collection the row was read
//! from rather than the one the connection opened on.
//!
//! The whole of the difficulty is in one line of each document: `q`, the filter
//! naming the row. A grid cell is text, and text alone cannot say whether
//! `65a1b2c3d4e5f60718293a4b` is an ObjectId or the string of its digits — and
//! the two match different documents, so guessing means an update that quietly
//! matches nothing. That is what `ColumnType::ObjectId` in `shape.rs` exists
//! for: the column says which, and this file writes `{"$oid": …}` or a bare
//! string accordingly. Every other type is spelled the same way, in Extended
//! JSON, so that an `int32` field stays an `int32` after an edit rather than
//! becoming whatever JSON's one number type happens to decode as.
//!
//! Two things are refused rather than written:
//!
//! - **A binary field.** Its cell shows how many bytes there were, not the
//!   bytes, so there is nothing on screen to write back.
//! - **`_id` in the `$set`.** MongoDB refuses to change one, and a refusal here
//!   says so before the statement leaves rather than after.

use bson::DateTime;
use bson::oid::ObjectId;
use dbconn::{ColumnInfo, EditedCell, RowEdits};

use crate::MongoError;

/// The command documents `edits` would take, in the order they have to be sent.
///
/// Updates, then inserts, then deletes — the order `dbedit` sends SQL in, and
/// for the reason it gives: a delete that went first could take a document an
/// update still needs.
///
/// `columns` is the collection's sampled shape, which is what says how each
/// cell's text should be spelled as BSON.
pub fn statements(edits: &RowEdits, columns: &[ColumnInfo]) -> Result<Vec<String>, MongoError> {
    if columns.is_empty() {
        return Err(refused(format!(
            "{} has no fields to change: nothing was read from it to say what it holds",
            qualified(edits)
        )));
    }
    let collection = quoted(&edits.relation);
    let database = quoted(&edits.schema);
    let mut written = Vec::new();
    for update in &edits.updates {
        let set = assignments(edits, &update.set, columns)?;
        if set.is_empty() {
            continue;
        }
        written.push(format!(
            "{{\"update\": {collection}, \"$db\": {database}, \
             \"updates\": [{{\"q\": {}, \"u\": {{\"$set\": {{{}}}}}, \"multi\": false}}]}}",
            naming(edits, &update.key, columns)?,
            set.join(", ")
        ));
    }
    for insert in &edits.inserts {
        written.push(format!(
            "{{\"insert\": {collection}, \"$db\": {database}, \"documents\": [{{{}}}]}}",
            // A new document with no `_id` is not an omission: MongoDB makes one,
            // and the row that comes back carries it. So an empty document is
            // written rather than refused -- `{}` inserts a document that is
            // nothing but its id, which is a document somebody may well mean.
            fields(edits, &insert.set, columns, Written::Anything)?.join(", ")
        ));
    }
    for delete in &edits.deletes {
        written.push(format!(
            "{{\"delete\": {collection}, \"$db\": {database}, \
             \"deletes\": [{{\"q\": {}, \"limit\": 1}}]}}",
            naming(edits, &delete.key, columns)?
        ));
    }
    Ok(written)
}

/// Whether `_id` may appear among the fields being written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Written {
    /// An insert, which chooses the new document's id or leaves it to the
    /// server.
    Anything,
    /// An update, where `_id` is what names the row and not part of what changes.
    NotTheKey,
}

/// The `$set` of an update.
fn assignments(
    edits: &RowEdits,
    set: &[EditedCell],
    columns: &[ColumnInfo],
) -> Result<Vec<String>, MongoError> {
    fields(edits, set, columns, Written::NotTheKey)
}

/// Cells as the `"field": value` pairs of a document.
fn fields(
    edits: &RowEdits,
    cells: &[EditedCell],
    columns: &[ColumnInfo],
    allowed: Written,
) -> Result<Vec<String>, MongoError> {
    let mut pairs = Vec::with_capacity(cells.len());
    for cell in cells {
        if allowed == Written::NotTheKey && cell.column == "_id" {
            return Err(refused(
                "_id is what names a document and MongoDB will not change one: \
                 insert the document you want and delete this one",
            ));
        }
        pairs.push(format!(
            "{}: {}",
            quoted(&cell.column),
            value(edits, cell, columns)?
        ));
    }
    Ok(pairs)
}

/// The filter naming one document, which is its key columns and their values.
fn naming(
    edits: &RowEdits,
    key: &[EditedCell],
    columns: &[ColumnInfo],
) -> Result<String, MongoError> {
    if key.is_empty() {
        return Err(refused(format!(
            "the row has nothing naming it, so there is no document for this change to reach in {}",
            qualified(edits)
        )));
    }
    let mut terms = Vec::with_capacity(key.len());
    for cell in key {
        // A key cell that is empty would match documents where the field is
        // absent or null -- which is every document that was never given one,
        // rather than the row somebody edited.
        if cell.value.is_none() {
            return Err(refused(format!(
                "the row's {} is empty, and an empty {} names no one document",
                cell.column, cell.column
            )));
        }
        terms.push(format!(
            "{}: {}",
            quoted(&cell.column),
            value(edits, cell, columns)?
        ));
    }
    Ok(format!("{{{}}}", terms.join(", ")))
}

/// One cell as the BSON its column holds, spelled in Extended JSON.
fn value(
    edits: &RowEdits,
    cell: &EditedCell,
    columns: &[ColumnInfo],
) -> Result<String, MongoError> {
    let Some(column) = columns.iter().find(|c| c.name == cell.column) else {
        return Err(refused(format!(
            "{} has no field {}",
            qualified(edits),
            cell.column
        )));
    };
    let Some(text) = cell.value.as_deref() else {
        return Ok("null".to_string());
    };
    let typed = text.trim();
    match column.data_type.as_str() {
        "bool" => match typed {
            "true" => Ok("true".to_string()),
            "false" => Ok("false".to_string()),
            _ => Err(wrong(text, "true or false")),
        },
        // Extended JSON rather than a bare number, and this is the reason: JSON
        // has one number type, so `5` written plainly comes back as an Int64 and
        // an edit to an Int32 field would change the field's type as a side
        // effect of changing its value.
        "int32" => match typed.parse::<i32>() {
            Ok(n) => Ok(format!("{{\"$numberInt\": \"{n}\"}}")),
            Err(_) => Err(wrong(text, "a whole number")),
        },
        "int64" => match typed.parse::<i64>() {
            Ok(n) => Ok(format!("{{\"$numberLong\": \"{n}\"}}")),
            Err(_) => Err(wrong(text, "a whole number")),
        },
        "float64" => match typed.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(format!("{{\"$numberDouble\": \"{n}\"}}")),
            _ => Err(wrong(text, "a number")),
        },
        "datetime" => match DateTime::parse_rfc3339_str(typed) {
            Ok(_) => Ok(format!("{{\"$date\": {}}}", quoted(typed))),
            Err(_) => Err(wrong(text, "a date as 2024-01-31T09:00:00Z")),
        },
        "objectid" => match ObjectId::parse_str(typed) {
            Ok(id) => Ok(format!("{{\"$oid\": \"{}\"}}", id.to_hex())),
            Err(_) => Err(wrong(text, "24 hex digits")),
        },
        // The cell already holds JSON -- it is how a nested document is shown --
        // so it goes in as the user has it rather than being parsed and printed
        // again, which would reorder its keys and reformat its numbers.
        "document" => match serde_json::from_str::<serde_json::Value>(typed) {
            Ok(_) => Ok(typed.to_string()),
            Err(_) => Err(wrong(text, "a JSON object or array")),
        },
        "binary" => Err(refused(format!(
            "{} shows how many bytes {} holds rather than the bytes, so there is nothing here to \
             write back",
            cell.column, cell.column
        ))),
        _ => Ok(quoted(text)),
    }
}

/// `schema.relation`, for the sentences somebody reads.
fn qualified(edits: &RowEdits) -> String {
    format!("{}.{}", edits.schema, edits.relation)
}

/// A string as JSON, with whatever is inside it escaped.
fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

fn wrong(text: &str, wanted: &str) -> MongoError {
    refused(format!("{text:?} is not {wanted}"))
}

fn refused(message: impl Into<String>) -> MongoError {
    MongoError::Statement {
        message: message.into(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, data_type: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
            is_primary_key: name == "_id",
            position: 1,
            default_value: None,
            computed: None,
        }
    }

    fn shape() -> Vec<ColumnInfo> {
        vec![
            column("_id", "objectid"),
            column("name", "text"),
            column("seats", "int32"),
            column("total", "float64"),
            column("open", "bool"),
            column("placed_at", "datetime"),
            column("address", "document"),
            column("thumbnail", "binary"),
        ]
    }

    fn edits(json: &str) -> RowEdits {
        serde_json::from_str(json).expect("the edits should parse")
    }

    fn written(json: &str) -> Vec<String> {
        statements(&edits(json), &shape()).expect("the statements should be written")
    }

    fn why(json: &str) -> String {
        statements(&edits(json), &shape())
            .expect_err("this change should be refused")
            .to_string()
    }

    const ID: &str = "65a1b2c3d4e5f60718293a4b";

    #[test]
    fn a_changed_field_is_an_update_naming_the_document_by_its_id() {
        let statements = written(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                              "set": [{{"column": "name", "value": "Ada"}}]}}],
                "inserts": [], "deletes": []}}"#
        ));
        assert_eq!(
            statements,
            vec![format!(
                r#"{{"update": "orders", "$db": "shop", "updates": [{{"q": {{"_id": {{"$oid": "{ID}"}}}}, "u": {{"$set": {{"name": "Ada"}}}}, "multi": false}}]}}"#
            )]
        );
    }

    #[test]
    fn the_command_is_the_first_field_of_every_document() {
        // MongoDB reads the command from the first key and this driver's own
        // `verb` does too, so a document whose fields were sorted -- which is
        // what building it through a map would do, `$db` sorting first -- would
        // be refused before it reached the server.
        let statements = written(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                              "set": [{{"column": "name", "value": "Ada"}}]}}],
                "inserts": [{{"set": [{{"column": "name", "value": "Grace"}}]}}],
                "deletes": [{{"key": [{{"column": "_id", "value": "{ID}"}}]}}]}}"#
        ));
        let verbs: Vec<&str> = statements
            .iter()
            .map(|s| s.split('"').nth(1).expect("a first key"))
            .collect();
        assert_eq!(verbs, vec!["update", "insert", "delete"]);
    }

    #[test]
    fn every_statement_parses_as_the_command_document_it_claims_to_be() {
        // The refusals below are about what this file will write; this is about
        // whether what it writes survives the driver's own parser, which is the
        // step between here and the server.
        for statement in written(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                              "set": [{{"column": "seats", "value": "4"}},
                                      {{"column": "total", "value": "12.5"}},
                                      {{"column": "open", "value": "true"}},
                                      {{"column": "placed_at", "value": "2024-01-31T09:00:00Z"}},
                                      {{"column": "address", "value": "{{\"city\": \"Taipei\"}}"}}]}}],
                "inserts": [], "deletes": []}}"#
        )) {
            crate::parse_statement(&statement).expect("the driver should read its own statement");
        }
    }

    #[test]
    fn a_number_keeps_the_width_its_field_has() {
        let statements = written(
            r#"{"schema": "shop", "relation": "orders",
                "updates": [], "deletes": [],
                "inserts": [{"set": [{"column": "seats", "value": "4"},
                                     {"column": "total", "value": "12.5"}]}]}"#,
        );
        assert!(
            statements[0].contains(r#""seats": {"$numberInt": "4"}"#),
            "an int32 field stays int32: {}",
            statements[0]
        );
        assert!(
            statements[0].contains(r#""total": {"$numberDouble": "12.5"}"#),
            "{}",
            statements[0]
        );
    }

    #[test]
    fn an_id_that_is_a_string_is_matched_as_a_string() {
        // The whole reason `shape.rs` tells an ObjectId from text: the same 24
        // characters in a text field name a different document.
        let text_id = vec![column("_id", "text")];
        let statements = statements(
            &edits(&format!(
                r#"{{"schema": "shop", "relation": "notes",
                    "updates": [], "inserts": [],
                    "deletes": [{{"key": [{{"column": "_id", "value": "{ID}"}}]}}]}}"#
            )),
            &text_id,
        )
        .expect("written");
        assert!(
            statements[0].contains(&format!(r#""q": {{"_id": "{ID}"}}"#)),
            "{}",
            statements[0]
        );
    }

    #[test]
    fn a_cleared_cell_sets_the_field_to_null_rather_than_removing_it() {
        let statements = written(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                              "set": [{{"column": "name", "value": null}}]}}],
                "inserts": [], "deletes": []}}"#
        ));
        assert!(
            statements[0].contains(r#""$set": {"name": null}"#),
            "{}",
            statements[0]
        );
    }

    #[test]
    fn a_new_document_may_leave_its_id_to_the_server() {
        let statements = written(
            r#"{"schema": "shop", "relation": "orders", "updates": [], "deletes": [],
                "inserts": [{"set": [{"column": "name", "value": "Grace"}]}]}"#,
        );
        assert_eq!(
            statements,
            vec![
                r#"{"insert": "orders", "$db": "shop", "documents": [{"name": "Grace"}]}"#
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_new_document_may_also_choose_its_own_id() {
        let statements = written(&format!(
            r#"{{"schema": "shop", "relation": "orders", "updates": [], "deletes": [],
                "inserts": [{{"set": [{{"column": "_id", "value": "{ID}"}}]}}]}}"#
        ));
        assert!(
            statements[0].contains(&format!(r#"{{"_id": {{"$oid": "{ID}"}}}}"#)),
            "{}",
            statements[0]
        );
    }

    #[test]
    fn changing_an_id_is_refused_rather_than_sent_for_the_server_to_refuse() {
        let message = why(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                              "set": [{{"column": "_id", "value": "{ID}"}}]}}],
                "inserts": [], "deletes": []}}"#
        ));
        assert!(message.contains("_id"), "{message}");
        assert!(message.contains("delete this one"), "{message}");
    }

    #[test]
    fn an_id_that_is_not_an_id_is_refused_before_it_matches_nothing() {
        let message = why(
            r#"{"schema": "shop", "relation": "orders", "updates": [], "inserts": [],
                "deletes": [{"key": [{"column": "_id", "value": "not-an-id"}]}]}"#,
        );
        assert!(message.contains("24 hex digits"), "{message}");
    }

    #[test]
    fn a_row_with_no_key_is_refused() {
        let message = why(
            r#"{"schema": "shop", "relation": "orders", "updates": [], "inserts": [],
                "deletes": [{"key": []}]}"#,
        );
        assert!(message.contains("nothing naming it"), "{message}");
    }

    #[test]
    fn an_empty_key_is_refused_rather_than_matching_every_document_without_one() {
        let message = why(
            r#"{"schema": "shop", "relation": "orders", "updates": [], "inserts": [],
                "deletes": [{"key": [{"column": "_id", "value": null}]}]}"#,
        );
        assert!(message.contains("names no one document"), "{message}");
    }

    #[test]
    fn a_binary_field_says_why_its_cell_cannot_be_written_back() {
        let message = why(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                              "set": [{{"column": "thumbnail", "value": "<9 bytes>"}}]}}],
                "inserts": [], "deletes": []}}"#
        ));
        assert!(message.contains("how many bytes"), "{message}");
    }

    #[test]
    fn a_field_the_collection_was_not_read_with_is_refused() {
        // `_extra` is the column this catches in practice: it is in the grid and
        // is not a field, so an edit to it would write a document holding the
        // JSON of the fields that did not fit anywhere.
        let message = why(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                              "set": [{{"column": "_extra", "value": "{{}}"}}]}}],
                "inserts": [], "deletes": []}}"#
        ));
        assert!(message.contains("has no field _extra"), "{message}");
    }

    #[test]
    fn text_that_is_not_the_type_its_field_holds_is_refused() {
        for (column, value, wanted) in [
            ("seats", "many", "a whole number"),
            ("total", "free", "a number"),
            ("open", "yes", "true or false"),
            ("placed_at", "yesterday", "a date as"),
            ("address", "Taipei", "a JSON object"),
        ] {
            let message = why(&format!(
                r#"{{"schema": "shop", "relation": "orders",
                    "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}],
                                  "set": [{{"column": "{column}", "value": "{value}"}}]}}],
                    "inserts": [], "deletes": []}}"#
            ));
            assert!(message.contains(wanted), "{column}: {message}");
        }
    }

    #[test]
    fn a_collection_nothing_was_read_from_says_so_rather_than_writing_a_bare_document() {
        let message = statements(
            &edits(
                r#"{"schema": "shop", "relation": "orders", "updates": [], "inserts": [],
                    "deletes": [{"key": [{"column": "_id", "value": "1"}]}]}"#,
            ),
            &[],
        )
        .expect_err("refused")
        .to_string();
        assert!(message.contains("no fields to change"), "{message}");
    }

    #[test]
    fn an_update_that_changes_nothing_writes_nothing() {
        let statements = written(&format!(
            r#"{{"schema": "shop", "relation": "orders",
                "updates": [{{"key": [{{"column": "_id", "value": "{ID}"}}], "set": []}}],
                "inserts": [], "deletes": []}}"#
        ));
        assert!(statements.is_empty(), "{statements:?}");
    }
}
