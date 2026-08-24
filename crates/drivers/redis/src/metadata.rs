//! What Redis can say about itself, in the shape the navigator expects.
//!
//! Seven of the nine calls answer without sending a command, which is a
//! statement about the database and not a gap here. Redis has no views, no
//! secondary indexes, no foreign keys, no constraints and no triggers, and its
//! relations — see the crate doc — are a fixed vocabulary of six types rather
//! than something to be listed. Asking the server would only be slower and would
//! give the same answer.
//!
//! `schemas` is the only call that sends anything, and the only one that can be
//! refused.
//!
//! The one place this reports less than it could: `estimated_rows`. `INFO
//! keyspace` gives the number of keys in each database, so the navigator could
//! show a count beside `db0` — but a relation here is a *type*, and there is no
//! command that counts the keys of one type without scanning the whole keyspace.
//! Reporting the database's total against each of the six relations would state
//! six times over that the database has more keys of that type than it does.
//! `None` means nothing has measured this, which is exactly the case.

use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    SchemaInfo, TriggerInfo, UniqueKeyInfo,
};
use redis::Value;

use crate::shape::{KeyType, TYPES, key_columns};
use crate::{DATABASES, RedisError, RedisSource};

impl RedisSource {
    /// The numbered databases, named the way `redis-cli` names them.
    ///
    /// Redis has a fixed number of databases decided at startup and no command
    /// that creates or drops one, so the navigator root is `db0` … `dbN-1` and
    /// the only question is what N is.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, RedisError> {
        let mut ask = redis::cmd("CONFIG");
        ask.arg("GET").arg("databases");
        let count = match self.ask(ask).await {
            Ok(reply) => setting(&reply).unwrap_or(DATABASES),
            // A refusal is not a failure. Every managed Redis disables `CONFIG`
            // — it is how a tenant would read the whole server's settings — and
            // an empty navigator would be a worse answer than a slightly wrong
            // one. So the number falls back to Redis's own compiled-in default
            // of 16, which is what a server that will not say is overwhelmingly
            // likely to have. The cost is visible and recoverable: on a server
            // started with fewer, the extra entries are there and selecting one
            // fails by name when the user clicks it; on a server started with
            // more, the ones past 16 are not offered and can still be reached by
            // typing `SELECT 20` in the editor.
            Err(_) => DATABASES,
        };
        Ok((0..count.max(1))
            // None of them is the server's own. Redis keeps its configuration
            // in the process rather than in a numbered database, so all sixteen
            // hold keys and nothing else.
            .map(|n| SchemaInfo {
                name: format!("db{n}"),
                is_system: false,
            })
            .collect())
    }

    /// The six value types, which are this driver's relations.
    ///
    /// All six, always, without asking whether the database holds any keys of
    /// each. Finding out costs a full scan of the keyspace per type — the one
    /// thing a navigator must never do — and a type with no keys is an empty
    /// relation rather than an absent one, exactly as a table with no rows is
    /// still a table. The six are the fixed vocabulary of the database and not
    /// something a user creates.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, RedisError> {
        Ok(TYPES
            .into_iter()
            .map(|of| RelationInfo {
                schema: schema.to_string(),
                name: of.name().to_string(),
                kind: RelationKind::Table,
                estimated_rows: None,
            })
            .collect())
    }

    /// The fixed columns a listing of keys of this type has.
    ///
    /// Read from the same function the grid builds its rows with, so the
    /// structure pane cannot describe a relation the browse does not produce. A
    /// name that is not one of the six answers with nothing rather than failing:
    /// a navigator works from a tree that can be one refresh out of date.
    pub async fn columns(
        &self,
        _schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, RedisError> {
        let Some(of) = KeyType::parse(relation) else {
            return Ok(Vec::new());
        };
        Ok(key_columns(Some(of))
            .into_iter()
            .enumerate()
            .map(|(at, column)| ColumnInfo {
                // The key is what addresses the row, it is unique within the
                // database, and looking it up is the only access path there is.
                // That is a primary key by every property that matters.
                is_primary_key: column.name == "key",
                name: column.name.to_string(),
                data_type: column.declared.to_string(),
                nullable: column.nullable,
                position: at as i32 + 1,
                default_value: None,
                computed: None,
            })
            .collect())
    }

    /// Always `None`. Redis has no views: there is no stored statement anywhere
    /// in it, and a relation here is a type rather than something anybody
    /// defined.
    pub async fn definition(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Option<String>, RedisError> {
        Ok(None)
    }

    /// Always empty, and without asking.
    ///
    /// Redis has one unique key and it is the key itself, which `columns`
    /// already reports as the primary key of every relation here. There is no
    /// second thing a value could be looked up by — that is the whole shape of a
    /// key-value store — so there is nothing for this to add.
    pub async fn unique_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, RedisError> {
        Ok(Vec::new())
    }

    /// Always empty, and without asking.
    ///
    /// Redis has no secondary indexes. The keyspace itself is the index — a hash
    /// table from key to value — and it is not something to list beside a
    /// relation, because it is the same one for all six of them. Applications
    /// that want an index build one out of sorted sets, which are ordinary keys
    /// and appear in the `zset` relation like any other.
    pub async fn indexes(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<IndexInfo>, RedisError> {
        Ok(Vec::new())
    }

    /// Always empty. Redis declares no references between keys; where an
    /// application stores one key's name inside another's value, nothing on the
    /// server knows it is a reference.
    pub async fn foreign_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, RedisError> {
        Ok(Vec::new())
    }

    /// Always empty, for the same reason as `foreign_keys`: there is nothing
    /// declared to look up from the other end either.
    pub async fn referenced_by(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, RedisError> {
        Ok(Vec::new())
    }

    /// Always empty, and this one was worth checking rather than assuming.
    ///
    /// MongoDB turned out to have check constraints under another name — a
    /// collection's JSON Schema validator rejects writes exactly as `CHECK`
    /// does. Redis has no counterpart. Nothing on the server refuses a write for
    /// the shape of its value; the closest thing, an eviction policy, is a
    /// property of the whole server rather than of anything a structure pane
    /// would show, and it does not reject a write, it discards an older key.
    pub async fn constraints(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<ConstraintInfo>, RedisError> {
        Ok(Vec::new())
    }

    /// Always empty. Redis has no triggers.
    ///
    /// Keyspace notifications are what people reach for instead, and they are not
    /// triggers for the same reason MongoDB's change streams are not: they are a
    /// pub/sub feed a client subscribes to, running in the client, with nothing
    /// registered against a key on the server. There is nothing here to list.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, RedisError> {
        Ok(Vec::new())
    }
}

/// The number in a `CONFIG GET` reply, which RESP3 delivers as a map of one
/// pair.
fn setting(reply: &Value) -> Option<i64> {
    match reply {
        Value::Map(pairs) => pairs
            .first()
            .and_then(|(_, value)| crate::shape::text(value).parse().ok()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::Value;

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    #[test]
    fn the_number_of_databases_is_read_from_the_map_config_get_answers_with() {
        let reply = Value::Map(vec![(bulk("databases"), bulk("16"))]);
        assert_eq!(setting(&reply), Some(16));
    }

    #[test]
    fn a_config_reply_that_is_not_a_setting_is_declined_rather_than_guessed_at() {
        assert_eq!(setting(&Value::Nil), None);
        assert_eq!(setting(&Value::Map(vec![])), None);
        assert_eq!(
            setting(&Value::Map(vec![(bulk("databases"), bulk("plenty"))])),
            None
        );
    }
}
