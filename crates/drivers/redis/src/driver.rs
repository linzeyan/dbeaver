//! `RedisSource` seen through the `Driver` trait.
//!
//! As thin as the others, and that is the result worth recording: a database
//! with no rows, no columns and no query language reached this boundary without
//! the trait growing a method or an option. The two places the fit is imperfect
//! are visible here rather than hidden — `browse` throws away two fields of
//! `Browse` because Redis has no spelling for either, and `transaction` refuses
//! every step there is.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, ColumnInfo, ConstraintInfo, Cursor as CursorApi, CursorCancel as CursorCancelApi,
    DbError, DbResult, Driver, IndexInfo, RelationInfo, RelationshipInfo, ResultStream, SchemaInfo,
    TriggerInfo, TxStep, UniqueKeyInfo,
};

use crate::{ArrowStream, Cursor, CursorCancel, RedisError, RedisSource};

impl From<RedisError> for DbError {
    fn from(e: RedisError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

/// The database number a schema name stands for.
///
/// `db3` is what `schemas` produced and `3` is what `SELECT` wants. A name that
/// is not one of those is passed through as it was given, so that the statement
/// names it and the server refuses it out loud — inventing a database number
/// here would browse a database nobody asked for.
fn database_of(schema: &str) -> &str {
    schema.strip_prefix("db").unwrap_or(schema)
}

#[async_trait]
impl Driver for RedisSource {
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(RedisSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(RedisSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(RedisSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(RedisSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(RedisSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always: the key is the only unique thing a Redis relation has, and
    /// `columns` already reports it as the primary key. See `metadata.rs`.
    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(RedisSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(RedisSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(RedisSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(RedisSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(RedisSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            RedisSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// A `SELECT` and a `SCAN`, on two lines, because Redis has no way to name a
    /// database inside a command.
    ///
    /// `SCAN 0` and not `SCAN`: the cursor to start from is a required argument,
    /// and a server asked without one answers "ERR invalid cursor".
    ///
    /// `filter` becomes the `MATCH` pattern and reaches the statement exactly as
    /// it was typed, as the trait requires. A glob pattern containing a space
    /// therefore has to be quoted by the person typing it — `"user name:*"` —
    /// which is the same thing `redis-cli` requires and is why the grammar has
    /// quoting at all. Quoting it here instead would put quotation marks around a
    /// pattern that already had them.
    ///
    /// `limit` becomes `COUNT`, which this driver reads as a row ceiling; the
    /// reading and its cost are argued at `Scan`.
    ///
    /// **`keys` and `order` are dropped, and nothing can be done with them.**
    /// `SCAN` returns keys in the order the hash table happens to hold them,
    /// which is not stable between two iterations and cannot be asked for. There
    /// is no `ORDER BY`, no index to read in order, and no key to sort on beyond
    /// the key itself — which `SCAN` will not sort by either. Sorting on this
    /// side would mean reading the whole keyspace before showing the first row,
    /// which is what paging exists to avoid. So a browse of Redis is not
    /// repeatable in the way a browse of a table is, and the honest thing is to
    /// say so here rather than to emit a statement that looks ordered and is not.
    fn browse(&self, what: &Browse<'_>) -> String {
        let mut scan = String::from("SCAN 0");
        if let Some(pattern) = what.filter.map(str::trim).filter(|f| !f.is_empty()) {
            scan.push_str(&format!(" MATCH {pattern}"));
        }
        scan.push_str(&format!(" TYPE {}", what.relation));
        if let Some(rows) = what.limit {
            scan.push_str(&format!(" COUNT {rows}"));
        }
        format!("SELECT {}\n{scan}", database_of(what.schema))
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            RedisSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(RedisSource::cancel(self).await?)
    }

    /// False, and not because this driver lacks a session connection — it has
    /// one, and statements do share it.
    ///
    /// Redis has no transaction a session can hold open. `MULTI` starts queueing
    /// commands, `EXEC` runs the queue, and between the two nothing has happened
    /// yet: a `GET` sent after `MULTI` answers `QUEUED` and not a value, so there
    /// is no reading back a change before deciding whether to keep it. That is a
    /// batch, and the trait's transaction is a session that can be read from
    /// while it is open. Offering Commit and Rollback over it would give a user
    /// two buttons whose meaning changes underneath them: `DISCARD` throws away
    /// commands that never ran, where Rollback undoes ones that did.
    ///
    /// `WATCH` does not close the gap. It is optimistic concurrency — the `EXEC`
    /// fails if a watched key changed — which is a retry loop an application
    /// writes, not something a Rollback button can stand for.
    fn transactional(&self) -> bool {
        false
    }

    /// Refused by name, every step, so that nobody is told a transaction is open
    /// when nothing is.
    async fn transaction(&self, step: &TxStep) -> DbResult<()> {
        let name = match step {
            TxStep::Begin => "BEGIN",
            TxStep::Commit => "COMMIT",
            TxStep::Rollback => "ROLLBACK",
            TxStep::Savepoint(_) => "SAVEPOINT",
            TxStep::RollbackTo(_) => "ROLLBACK TO",
            TxStep::Release(_) => "RELEASE",
        };
        Err(DbError::new(format!(
            "Redis has no transaction a session can hold open, so {name} has nothing to do: \
             MULTI queues commands and EXEC runs them all at once, and a command between the \
             two answers QUEUED rather than a value"
        )))
    }
}

#[async_trait]
impl ResultStream for ArrowStream {
    fn schema(&self) -> SchemaRef {
        ArrowStream::schema(self)
    }

    fn rows_affected(&self) -> Option<u64> {
        ArrowStream::rows_affected(self)
    }

    async fn next_batch(&mut self) -> DbResult<Option<RecordBatch>> {
        Ok(ArrowStream::next_batch(self).await?)
    }
}

#[async_trait]
impl CursorApi for Cursor {
    fn schema(&self) -> SchemaRef {
        Cursor::schema(self)
    }

    async fn fetch(&mut self) -> DbResult<Option<RecordBatch>> {
        Ok(Cursor::fetch(self).await?)
    }

    fn canceller(&self) -> Box<dyn CursorCancelApi> {
        Box::new(Cursor::canceller(self))
    }

    async fn close(&mut self) -> DbResult<()> {
        Ok(Cursor::close(self).await?)
    }
}

#[async_trait]
impl CursorCancelApi for CursorCancel {
    async fn cancel(&self) -> DbResult<()> {
        Ok(CursorCancel::cancel(self).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::database_of;

    #[test]
    fn a_schema_name_becomes_the_number_select_wants() {
        assert_eq!(database_of("db0"), "0");
        assert_eq!(database_of("db15"), "15");
    }

    #[test]
    fn a_name_this_driver_did_not_produce_is_passed_on_for_the_server_to_refuse() {
        // Better than a browse that quietly reads db0 because the name did not
        // parse: `SELECT public` fails and says so.
        assert_eq!(database_of("public"), "public");
    }
}
