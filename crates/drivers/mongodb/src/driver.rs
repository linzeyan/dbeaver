//! `MongoSource` seen through the `Driver` trait.
//!
//! As thin as the other two, which is the result worth recording: a document
//! database reached the same boundary without the trait having to bend. The one
//! place the fit is imperfect is not visible here at all — `query`'s parameter
//! is named `sql` and what this driver receives is a JSON command document. The
//! name is wrong; the shape is not.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, ColumnInfo, ConstraintInfo, Cursor as CursorApi, CursorCancel as CursorCancelApi,
    DbError, DbResult, Driver, IndexInfo, RelationInfo, RelationshipInfo, ResultStream, SchemaInfo,
    ServerInfo, TriggerInfo, TxStep, UniqueKeyInfo,
};

use crate::{ArrowStream, Cursor, CursorCancel, MongoError, MongoSource};

impl From<MongoError> for DbError {
    fn from(e: MongoError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

#[async_trait]
impl Driver for MongoSource {
    /// The product, without a version. MongoDB states its build in reply to
    /// `buildInfo`, which answers with a flat document rather than a cursor, and
    /// this driver's reader reads cursors. Naming the product is what can be
    /// answered honestly until it reads one.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new("MongoDB", ""))
    }
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(MongoSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(MongoSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(MongoSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(MongoSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(MongoSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always, although `indexes` does report MongoDB's unique ones: a
    /// field this driver arrived at by sampling documents cannot promise the
    /// next document will have it. See `metadata.rs`.
    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(MongoSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(MongoSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(MongoSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(MongoSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(MongoSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            MongoSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// A `find` command, because this database has no SELECT to write.
    ///
    /// The database is named with `$db`, which is the field MongoDB's own wire
    /// protocol carries it in — not a convention invented here, and the reason a
    /// browse can reach a collection outside the one the connection opened on.
    ///
    /// `filter` and `order` are the user's own JSON — a filter document and a
    /// sort document — and go in unaltered. Text that is not JSON produces a
    /// statement this driver refuses at `query`, pointing at the character it
    /// stopped on.
    fn browse(&self, what: &Browse<'_>) -> String {
        let quoted = |text: &str| serde_json::Value::String(text.to_string()).to_string();
        let mut parts = vec![
            format!("\"find\": {}", quoted(what.relation)),
            format!("\"$db\": {}", quoted(what.schema)),
        ];
        if let Some(filter) = what.filter.map(str::trim).filter(|f| !f.is_empty()) {
            parts.push(format!("\"filter\": {filter}"));
        }
        // The user's sort document, or one built from the key columns. Not both:
        // merging them would mean parsing the user's JSON in order to hand it
        // back, and a sort this side rewrote is no longer the one they wrote.
        if let Some(sort) = what.order.map(str::trim).filter(|o| !o.is_empty()) {
            parts.push(format!("\"sort\": {sort}"));
        } else if !what.keys.is_empty() {
            let terms: Vec<String> = what
                .keys
                .iter()
                .map(|key| format!("{}: 1", quoted(key)))
                .collect();
            parts.push(format!("\"sort\": {{{}}}", terms.join(", ")));
        }
        if let Some(rows) = what.limit {
            parts.push(format!("\"limit\": {rows}"));
        }
        format!("{{{}}}", parts.join(", "))
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            MongoSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(MongoSource::cancel(self).await?)
    }

    /// Not yet: MongoDB has multi-document transactions, and reaching them
    /// means threading a `ClientSession` through every operation and running
    /// against a replica set or a sharded cluster — a standalone `mongod`,
    /// which is what a laptop runs, refuses them outright.
    fn transactional(&self) -> bool {
        false
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new(
            "MongoDB transactions need a replica set and a session this driver does not hold",
        ))
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
