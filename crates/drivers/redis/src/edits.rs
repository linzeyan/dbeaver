//! A grid's staged changes, as the Redis commands that would apply them.
//!
//! The counterpart of `browse` in `driver.rs`: that one writes the `SCAN` a
//! listing of keys comes from, and this one writes the `SET`, `EXPIRE` and `DEL`
//! that put a change back. Both are here rather than above the driver because
//! Redis has no SQL for the layer above to compose, and both begin with the
//! `SELECT` that puts the command on the database the row was read from.
//!
//! Three commands and no more, which is a decision rather than a stopping point.
//! A key-value store's row is a key, a lifetime and a value, and those three are
//! exactly what can be said about one without inventing a meaning:
//!
//! - **A collection's value is a rendering.** A hash arrives in the grid as a
//!   JSON object because a cell holds one piece of text; putting one back would
//!   be a `DEL` and a rebuild, which is not what pressing Set says it does, and
//!   is destructive in a way nothing on screen would warn about. So the four
//!   collection types and streams refuse the value column, naming the command
//!   that would do it.
//! - **A key's name is not a cell.** Renaming is `RENAME`, which overwrites
//!   whatever is already at the new name without asking.
//! - **`size` and `type` are read off the value**, not set on it.
//!
//! Each refusal happens before anything is sent, so an edit this database cannot
//! express costs a message rather than half a write.

use dbconn::{EditedCell, RowEdits};

use crate::RedisError;
use crate::shape::{KeyType, VALUE};

/// The commands `edits` would take, in the order they have to be sent.
///
/// Updates, then inserts, then deletes — the order `dbedit` sends SQL in, and
/// for the reason it gives: a delete that went first could take a key an update
/// still needs, and an insert reusing a name a delete is about to free is the
/// one ordering that cannot work.
pub fn statements(edits: &RowEdits) -> Result<Vec<String>, RedisError> {
    let Some(of) = KeyType::parse(&edits.relation) else {
        return Err(refused(format!(
            "{} is not one of the six key types a Redis database holds",
            edits.relation
        )));
    };
    let mut written = Vec::new();
    for update in &edits.updates {
        let key = named(&update.key)?;
        for cell in &update.set {
            written.push(changed(of, &key, cell)?);
        }
    }
    for insert in &edits.inserts {
        written.extend(added(of, &insert.set)?);
    }
    for delete in &edits.deletes {
        written.push(command(&["DEL", &named(&delete.key)?]));
    }
    // The database the rows were read from, on every command rather than once
    // at the front: each of these is sent on its own through `query`, and a
    // `SELECT` in the statement before it would have moved the session for the
    // ones after it and left it moved afterwards.
    let database = edits.schema.strip_prefix("db").unwrap_or(&edits.schema);
    Ok(written
        .into_iter()
        .map(|line| format!("SELECT {database}\n{line}"))
        .collect())
}

/// One changed cell of a key that is already there.
fn changed(of: KeyType, key: &str, cell: &EditedCell) -> Result<String, RedisError> {
    let typed = cell
        .value
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    match cell.column.as_str() {
        "ttl" => match typed {
            Some(seconds) => {
                seconds.parse::<i64>().map_err(|_| {
                    refused(format!(
                        "a time to live is a number of seconds, not {seconds:?}"
                    ))
                })?;
                Ok(command(&["EXPIRE", key, seconds]))
            }
            // Cleared means "no expiry", which is `PERSIST`. Not `EXPIRE 0`,
            // which is how Redis spells "delete this key now" — the one wrong
            // answer here would take the row instead of its deadline.
            None => Ok(command(&["PERSIST", key])),
        },
        VALUE if of == KeyType::String => match cell.value.as_deref() {
            Some(value) => Ok(command(&["SET", key, value])),
            // Emptied is a real value — `SET k ""` — and cleared is not. Redis
            // has no key that exists holding nothing, so the row somebody meant
            // to remove is removed by removing the row.
            None => Err(refused(
                "a Redis string cannot hold nothing: set it to an empty value, \
                 or delete the row",
            )),
        },
        VALUE => Err(refused(format!(
            "the value shown for a {kind} is the whole {kind} rendered as JSON, and no single \
             command puts one back — {writer} in the editor writes into it",
            kind = of.name(),
            writer = writer(of)
        ))),
        "key" => Err(refused(
            "a key's name is changed with RENAME, which replaces whatever is already at the new \
             name without asking — type it in the editor if that is what you mean",
        )),
        "size" => Err(refused(
            "size is how many things the key held when it was read, not something to set",
        )),
        "type" => Err(refused(
            "a key's type is decided by what was put in it and cannot be changed in place",
        )),
        other => Err(refused(format!("{other} is not a column of this relation"))),
    }
}

/// A key that was not there.
fn added(of: KeyType, set: &[EditedCell]) -> Result<Vec<String>, RedisError> {
    if of != KeyType::String {
        return Err(refused(format!(
            "a {kind} comes into being when the first thing is put in it — {writer} in the editor \
             — so there is no empty {kind} for a new row to be",
            kind = of.name(),
            writer = writer(of)
        )));
    }
    let Some(key) = filled(set, "key") else {
        return Err(refused("a new key needs a name in the key column"));
    };
    let Some(value) = set
        .iter()
        .find(|c| c.column == VALUE)
        .and_then(|c| c.value.as_deref())
    else {
        return Err(refused(
            "a new key needs a value; Redis has no empty string key",
        ));
    };
    let mut written = vec![command(&["SET", key, value])];
    if let Some(seconds) = filled(set, "ttl") {
        seconds.parse::<i64>().map_err(|_| {
            refused(format!(
                "a time to live is a number of seconds, not {seconds:?}"
            ))
        })?;
        // After the `SET` and not folded into it as `SET k v EX n`: the two are
        // written separately so that a refused deadline leaves a key somebody
        // can see rather than no key and an error about the deadline.
        written.push(command(&["EXPIRE", key, seconds]));
    }
    Ok(written)
}

/// The key a row is addressed by.
///
/// Exactly one cell, and it has to be the key column. `columns` reports `key` as
/// the primary key of all six relations and nothing else is unique, so a request
/// naming anything else was not built from this driver's metadata.
fn named(key: &[EditedCell]) -> Result<String, RedisError> {
    match key {
        [cell] if cell.column == "key" => match cell.value.as_deref() {
            Some(name) if !name.is_empty() => Ok(name.to_string()),
            _ => Err(refused("the row has no key to address it by")),
        },
        _ => Err(refused(
            "a Redis row is addressed by its key and by nothing else",
        )),
    }
}

/// A cell's value where it was typed into and is not blank.
fn filled<'a>(set: &'a [EditedCell], column: &str) -> Option<&'a str> {
    set.iter()
        .find(|c| c.column == column)?
        .value
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// The command that writes into this type, for the refusals that name it.
fn writer(of: KeyType) -> &'static str {
    match of {
        KeyType::String => "SET",
        KeyType::Hash => "HSET",
        KeyType::List => "RPUSH",
        KeyType::Set => "SADD",
        KeyType::ZSet => "ZADD",
        KeyType::Stream => "XADD",
    }
}

/// One command, in the syntax this driver's own parser reads.
///
/// Every argument after the command name is quoted, whether or not it needs to
/// be. Redis arguments are byte strings and quoting one changes nothing about
/// what arrives; deciding case by case would mean a rule about which characters
/// are safe, kept in step with `split_args` by hand.
fn command(parts: &[&str]) -> String {
    let mut line = String::from(parts[0]);
    for part in &parts[1..] {
        line.push(' ');
        line.push('"');
        for c in part.chars() {
            if c == '"' || c == '\\' {
                line.push('\\');
            }
            line.push(c);
        }
        line.push('"');
    }
    line
}

fn refused(message: impl Into<String>) -> RedisError {
    RedisError::Statement {
        message: message.into(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(column: &str, value: Option<&str>) -> EditedCell {
        EditedCell {
            column: column.to_string(),
            value: value.map(str::to_string),
        }
    }

    fn edits(relation: &str) -> RowEdits {
        RowEdits {
            schema: "db3".to_string(),
            relation: relation.to_string(),
            updates: Vec::new(),
            inserts: Vec::new(),
            deletes: Vec::new(),
        }
    }

    fn message(result: Result<Vec<String>, RedisError>) -> String {
        match result {
            Ok(written) => panic!("expected a refusal, got {written:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_changed_string_is_a_set_on_the_database_the_row_came_from() {
        let mut edits = edits("string");
        edits.updates.push(dbconn::RowUpdate {
            key: vec![cell("key", Some("greeting"))],
            set: vec![cell("value", Some("hello"))],
        });
        assert_eq!(
            statements(&edits).expect("a command"),
            vec!["SELECT 3\nSET \"greeting\" \"hello\""]
        );
    }

    /// The `SELECT` is on every command rather than sent once, because each of
    /// these goes to the server on its own.
    #[test]
    fn every_command_carries_the_database_it_belongs_to() {
        let mut edits = edits("string");
        edits.deletes.push(dbconn::RowDelete {
            key: vec![cell("key", Some("a"))],
        });
        edits.deletes.push(dbconn::RowDelete {
            key: vec![cell("key", Some("b"))],
        });
        let written = statements(&edits).expect("commands");
        assert_eq!(written.len(), 2);
        assert!(written.iter().all(|line| line.starts_with("SELECT 3\n")));
    }

    #[test]
    fn a_key_with_a_quote_in_it_is_escaped_rather_than_ending_the_argument() {
        let mut edits = edits("string");
        edits.updates.push(dbconn::RowUpdate {
            key: vec![cell("key", Some(r#"say "hi""#))],
            set: vec![cell("value", Some(r"a\b"))],
        });
        assert_eq!(
            statements(&edits).expect("a command"),
            vec![
                r#"SELECT 3
SET "say \"hi\"" "a\\b""#
            ]
        );
    }

    /// Redis will happily hold a key named `""`, which is what makes this worth
    /// refusing: an empty key cell is a row that did not come from a listing,
    /// and writing it would put somebody's edit under a name they cannot see
    /// instead of failing.
    #[test]
    fn a_row_with_a_blank_key_is_refused_rather_than_written_to_the_empty_key() {
        let mut blank = edits("string");
        blank.updates.push(dbconn::RowUpdate {
            key: vec![cell("key", Some(""))],
            set: vec![cell("value", Some("hello"))],
        });
        assert!(
            message(statements(&blank)).contains("no key to address it by"),
            "an empty name is not a name"
        );

        let mut cleared = edits("string");
        cleared.deletes.push(dbconn::RowDelete {
            key: vec![cell("key", None)],
        });
        assert!(
            message(statements(&cleared)).contains("no key to address it by"),
            "and neither is a cleared one"
        );
    }

    /// Cleared means "no deadline", and `EXPIRE 0` is how Redis spells "gone
    /// now" — the one wrong answer here would delete the row.
    #[test]
    fn clearing_a_time_to_live_persists_the_key_rather_than_expiring_it_at_once() {
        let mut edits = edits("hash");
        edits.updates.push(dbconn::RowUpdate {
            key: vec![cell("key", Some("session"))],
            set: vec![cell("ttl", None)],
        });
        assert_eq!(
            statements(&edits).expect("a command"),
            vec!["SELECT 3\nPERSIST \"session\""]
        );
    }

    #[test]
    fn a_time_to_live_is_a_number_of_seconds_and_nothing_else() {
        let mut edits = edits("string");
        edits.updates.push(dbconn::RowUpdate {
            key: vec![cell("key", Some("k"))],
            set: vec![cell("ttl", Some("tomorrow"))],
        });
        assert!(message(statements(&edits)).contains("number of seconds"));
    }

    /// The deadline of a collection is editable even though its value is not:
    /// a lifetime belongs to the key, and every key has one.
    #[test]
    fn a_collection_takes_a_deadline_even_though_it_does_not_take_a_value() {
        let mut edits = edits("zset");
        edits.updates.push(dbconn::RowUpdate {
            key: vec![cell("key", Some("leaderboard"))],
            set: vec![cell("ttl", Some("60"))],
        });
        assert_eq!(
            statements(&edits).expect("a command"),
            vec!["SELECT 3\nEXPIRE \"leaderboard\" \"60\""]
        );
    }

    /// A refusal that names the command that would do it, rather than one that
    /// says the database cannot be written to — it can, and this says how.
    #[test]
    fn a_collections_value_is_refused_by_naming_what_would_write_into_it() {
        for (relation, writer) in [("hash", "HSET"), ("list", "RPUSH"), ("set", "SADD")] {
            let mut edits = edits(relation);
            edits.updates.push(dbconn::RowUpdate {
                key: vec![cell("key", Some("k"))],
                set: vec![cell("value", Some("[1,2]"))],
            });
            let said = message(statements(&edits));
            assert!(said.contains(writer), "{relation}: {said}");
            assert!(said.contains("rendered as JSON"), "{relation}: {said}");
        }
    }

    #[test]
    fn renaming_a_key_is_refused_because_rename_would_overwrite_what_is_there() {
        let mut edits = edits("string");
        edits.updates.push(dbconn::RowUpdate {
            key: vec![cell("key", Some("old"))],
            set: vec![cell("key", Some("new"))],
        });
        assert!(message(statements(&edits)).contains("RENAME"));
    }

    #[test]
    fn a_new_string_is_a_set_and_its_deadline_a_second_command() {
        let mut edits = edits("string");
        edits.inserts.push(dbconn::RowInsert {
            set: vec![
                cell("key", Some("token")),
                cell("value", Some("abc")),
                cell("ttl", Some("30")),
            ],
        });
        assert_eq!(
            statements(&edits).expect("commands"),
            vec![
                "SELECT 3\nSET \"token\" \"abc\"",
                "SELECT 3\nEXPIRE \"token\" \"30\""
            ]
        );
    }

    #[test]
    fn a_new_collection_is_refused_because_there_is_no_empty_one_to_make() {
        let mut edits = edits("list");
        edits.inserts.push(dbconn::RowInsert {
            set: vec![cell("key", Some("queue")), cell("value", Some("[]"))],
        });
        let said = message(statements(&edits));
        assert!(said.contains("RPUSH"), "{said}");
    }

    #[test]
    fn a_row_addressed_by_anything_but_its_key_is_refused() {
        let mut edits = edits("string");
        edits.deletes.push(dbconn::RowDelete {
            key: vec![cell("value", Some("hello"))],
        });
        assert!(message(statements(&edits)).contains("addressed by its key"));
    }

    #[test]
    fn a_relation_that_is_not_one_of_the_six_types_is_refused() {
        let mut edits = edits("orders");
        edits.deletes.push(dbconn::RowDelete {
            key: vec![cell("key", Some("k"))],
        });
        assert!(message(statements(&edits)).contains("six key types"));
    }
}
