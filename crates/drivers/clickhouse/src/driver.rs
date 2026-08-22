//! `ChSource` seen through the `Driver` trait.
//!
//! A thin layer on purpose. Everything here either forwards a call or converts
//! an error, and the day it starts doing more than that is the day the trait has
//! stopped fitting this database.
//!
//! One thing does not come through, and it is the same thing in both directions:
//! `ChSource::storage` has no counterpart on the trait, so the engine, the
//! sorting key and the partition key stop at this boundary. `RelationInfo` has
//! no field for any of them and the trait is not this driver's to widen.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, TriggerInfo, TxStep,
    UniqueKeyInfo, scalar_text,
};

use crate::{ChError, ChSource, Rows, RowsCancel};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error is thrown away.
///
/// The position and the cancellation flag are read off the server's exception at
/// the point where the statement text is still known — see
/// `ChError::from_server` — rather than recovered here by reading the prose
/// back, which is how a caret ends up pointing at whatever the message happened
/// to contain.
impl From<ChError> for DbError {
    fn from(e: ChError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

#[async_trait]
impl Driver for ChSource {
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new(
            "ClickHouse",
            scalar_text(self, "SELECT version()").await?,
        ))
    }
    /// The same as MySQL: a ClickHouse database holds tables directly, and
    /// `schemas()` reads `system.databases`. There is nothing above it.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(ChSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(ChSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(ChSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(ChSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(ChSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always: ClickHouse enforces uniqueness nowhere, its primary key
    /// included. See `metadata.rs`.
    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(ChSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(ChSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(ChSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(ChSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(ChSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            ChSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM …`. The schema is ClickHouse's database, and the two-part
    /// name is what reaches a table outside the one the connection opened on.
    fn browse(&self, what: &Browse<'_>) -> String {
        what.sql(&dbsql::CLICKHOUSE)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            ChSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(ChSource::cancel(self).await?)
    }

    /// No, and not for want of a session connection. ClickHouse's transactions
    /// are experimental, are off unless the server was started with them on, and
    /// cover one INSERT rather than a session's worth of statements. Answering
    /// yes here would put a Commit button on screen for something that is not
    /// one.
    ///
    /// Cancel reaches the statement: one `KILL QUERY` naming every live statement
    /// of this session.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: true,
            switches_database: false,
        }
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new("ClickHouse has no transactions to control"))
    }
}

/// `Rows` is both, because against ClickHouse a cursor and a result stream are
/// the same object read the same way. Nothing is lost by saying so: the two
/// traits ask for the same forward read, and the only difference in the shared
/// vocabulary is which of them the caller wanted.
#[async_trait]
impl ResultStream for Rows {
    fn schema(&self) -> SchemaRef {
        Rows::schema(self)
    }

    fn rows_affected(&self) -> Option<u64> {
        Rows::rows_affected(self)
    }

    async fn next_batch(&mut self) -> DbResult<Option<RecordBatch>> {
        Ok(Rows::next_page(self).await?)
    }
}

#[async_trait]
impl CursorApi for Rows {
    fn schema(&self) -> SchemaRef {
        Rows::schema(self)
    }

    async fn fetch(&mut self) -> DbResult<Option<RecordBatch>> {
        Ok(Rows::next_page(self).await?)
    }

    fn canceller(&self) -> Box<dyn CursorCancelApi> {
        Box::new(Rows::canceller(self))
    }

    async fn close(&mut self) -> DbResult<()> {
        Ok(Rows::close(self).await?)
    }
}

#[async_trait]
impl CursorCancelApi for RowsCancel {
    async fn cancel(&self) -> DbResult<()> {
        Ok(RowsCancel::cancel(self).await?)
    }
}
