//! `SqliteSource` seen through the `Driver` trait.
//!
//! Thin, like PostgreSQL's, with two places where it is not quite a forward and
//! both are the trait accommodating the other database. `cancel` is async here
//! and has nothing to await; a cursor is the same object a query returns, so its
//! `close` is a channel being shut rather than a statement sent anywhere.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, ColumnInfo, ConstraintInfo, Cursor as CursorApi, CursorCancel as CursorCancelApi,
    DbError, DbResult, Driver, IndexInfo, RelationInfo, RelationshipInfo, ResultStream, SchemaInfo,
    TriggerInfo, TxStep, UniqueKeyInfo,
};

use crate::{ArrowStream, Cursor, CursorCancel, SqliteError, SqliteSource};

impl From<SqliteError> for DbError {
    fn from(e: SqliteError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

#[async_trait]
impl Driver for SqliteSource {
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(SqliteSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(SqliteSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(SqliteSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(SqliteSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(SqliteSource::indexes(self, schema, relation).await?)
    }

    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(SqliteSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(SqliteSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(SqliteSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(SqliteSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(SqliteSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            SqliteSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM …`. SQLite takes double quotes, backticks and brackets;
    /// the standard's spelling is what it is given, so the statement reads as
    /// SQL rather than as SQLite.
    fn browse(&self, what: &Browse<'_>) -> String {
        what.sql(&dbsql::SQLITE)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            SqliteSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    /// Async with nothing to await. Interrupting SQLite sets a flag in this
    /// process and returns; the signature belongs to PostgreSQL, where the
    /// request is a round trip on a connection of its own, and a synchronous one
    /// would leave that driver blocking a runtime thread inside it.
    async fn cancel(&self) -> DbResult<()> {
        SqliteSource::cancel(self);
        Ok(())
    }

    /// Statements run on the session connection, which is what a transaction
    /// needs in order to span two of them.
    fn transactional(&self) -> bool {
        true
    }

    async fn transaction(&self, step: &TxStep) -> DbResult<()> {
        Ok(SqliteSource::transaction(self, step).await?)
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
        CursorCancel::cancel(self);
        Ok(())
    }
}
