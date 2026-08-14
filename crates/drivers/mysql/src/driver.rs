//! `MySqlSource` seen through the `Driver` trait.
//!
//! A thin layer on purpose. Everything here either forwards a call or converts
//! an error, and the day it starts doing more than that is the day the trait has
//! stopped fitting this database.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    ColumnInfo, ConstraintInfo, Cursor as CursorApi, CursorCancel as CursorCancelApi, DbError,
    DbResult, Driver, IndexInfo, RelationInfo, RelationshipInfo, ResultStream, SchemaInfo,
    TriggerInfo,
};

use crate::{ArrowStream, Cursor, CursorCancel, MySqlError, MySqlSource};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error is thrown away.
///
/// The position and the cancellation flag are facts about the failure rather
/// than parts of the sentence describing it. On this database one of them is
/// always absent — MySQL's parse error names the text it stopped at and never
/// where that text was — and it is left absent rather than reconstructed by
/// searching the statement, because a caret in a plausible wrong place is worse
/// than none.
impl From<MySqlError> for DbError {
    fn from(e: MySqlError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

#[async_trait]
impl Driver for MySqlSource {
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(MySqlSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(MySqlSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(MySqlSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(MySqlSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(MySqlSource::indexes(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(MySqlSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(MySqlSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(MySqlSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(MySqlSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            MySqlSource::query(self, statement, batch_rows).await?,
        ))
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            MySqlSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(MySqlSource::cancel(self).await?)
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
