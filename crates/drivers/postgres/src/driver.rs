//! `PgSource` seen through the `Driver` trait.
//!
//! A thin layer on purpose. Everything here either forwards a call or converts
//! an error, and the day it starts doing more than that is the day the trait has
//! stopped fitting this database.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, RoutineInfo, SchemaInfo, ServerInfo, TriggerInfo,
    TxStep, UniqueKeyInfo, scalar_text,
};

use crate::{ArrowStream, Cursor, CursorCancel, PgError, PgSource};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error is thrown away.
///
/// The position and the cancellation flag are facts about the failure rather
/// than parts of the sentence describing it, and recovering either by reading
/// the prose back is how a caret ends up pointing at whatever the message
/// happened to contain.
impl From<PgError> for DbError {
    fn from(e: PgError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

#[async_trait]
impl Driver for PgSource {
    /// `SELECT version()`, whose first word is the product's own name — which is
    /// how a connection opened as `postgres://` reports CockroachDB or
    /// GreptimeDB as itself instead of as PostgreSQL.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::from_banner(
            &scalar_text(self, "SELECT version()").await?,
        ))
    }
    /// One of the two drivers here with a database level worth drawing: schemas
    /// are real schemas, and the databases above them are unreachable from this
    /// session.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(Some(PgSource::databases(self).await?))
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(PgSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(PgSource::relations(self, schema).await?)
    }

    async fn routines(&self, schema: &str) -> DbResult<Vec<RoutineInfo>> {
        Ok(PgSource::routines(self, schema).await?)
    }

    async fn routine_definition(&self, schema: &str, id: &str) -> DbResult<Option<String>> {
        Ok(PgSource::routine_definition(self, schema, id).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(PgSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(PgSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(PgSource::indexes(self, schema, relation).await?)
    }

    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(PgSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(PgSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(PgSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(PgSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(PgSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            PgSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM …`, with the standard's quoting, which is PostgreSQL's own.
    fn browse(&self, what: &Browse<'_>) -> String {
        what.sql(&dbsql::POSTGRES)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            PgSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        // The reach is dropped rather than carried into the trait: it differs
        // per driver — some have one connection to name — and a front end that
        // branched on it would be reading the pool, not the cancellation.
        PgSource::cancel(self).await?;
        Ok(())
    }

    /// Statements run on a connection of their own here, which is what a
    /// transaction needs to span them.
    ///
    /// Cancel reaches the statement: it opens a connection of its own and names
    /// this session's backend, which is what the postmaster signals.
    ///
    /// A database is not somewhere this session can move to. PostgreSQL settles
    /// it at connect and there is no statement that changes it, so the list this
    /// driver reports is a list of connections to open.
    ///
    /// Routines are reported: `pg_proc` holds functions and procedures, and
    /// `pg_get_functiondef` hands back the source of either.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: true,
            cancel_stops_the_statement: true,
            switches_database: false,
            schema_is_the_database: false,
            reports_routines: true,
        }
    }

    async fn transaction(&self, step: &TxStep) -> DbResult<()> {
        Ok(PgSource::transaction(self, step).await?)
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
