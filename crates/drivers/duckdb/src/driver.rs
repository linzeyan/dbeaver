//! `DuckSource` seen through the `Driver` trait.
//!
//! A forward everywhere except `cancel`, which is async here and has nothing to
//! await — the signature belongs to PostgreSQL, where the request travels to a
//! server on a connection of its own. Same accommodation `driver-sqlite` makes,
//! and for the same reason: a synchronous signature would leave the networked
//! driver blocking a runtime thread inside it.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, TriggerInfo, TxStep,
    UniqueKeyInfo, scalar_text,
};

use crate::{ArrowStream, Cursor, CursorCancel, DuckError, DuckSource};

impl From<DuckError> for DbError {
    fn from(e: DuckError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

#[async_trait]
impl Driver for DuckSource {
    /// DuckDB writes its version with a leading `v`, and it is kept: the version
    /// is the server's own spelling and this client does not tidy it.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new(
            "DuckDB",
            scalar_text(self, "SELECT version()").await?,
        ))
    }
    /// The attached databases, which is a level DuckDB genuinely has.
    ///
    /// Never `None`, even for a session with nothing attached: there is always
    /// at least the database the connection opened on, and the level is real
    /// whether or not a second one has been added to it. What makes it unlike
    /// the level the two servers report is where the entries lead — an attached
    /// database is a catalog on this connection and not somewhere to open
    /// another one, which is what `capabilities().switches_database` says.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(Some(DuckSource::databases(self).await?))
    }

    async fn use_database(&self, name: &str) -> DbResult<()> {
        Ok(DuckSource::use_database(self, name).await?)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(DuckSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(DuckSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(DuckSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(DuckSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(DuckSource::indexes(self, schema, relation).await?)
    }

    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(DuckSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(DuckSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(DuckSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(DuckSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(DuckSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            DuckSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM …`, in PostgreSQL's spelling, which DuckDB follows.
    ///
    /// Named in all three parts, and that is not decoration. A browse is read
    /// through a cursor, a cursor runs on a connection cloned for it, and `USE`
    /// is per connection — so a two-part name on a session that has moved would
    /// resolve in the database the clone opened on, which is a different table
    /// or none. Each part is quoted separately, so a database called
    /// `sales.2024` is one identifier rather than two.
    fn browse(&self, what: &Browse<'_>) -> String {
        let name = format!(
            "{}.{}.{}",
            dbsql::DUCKDB.quote(&self.current_database()),
            dbsql::DUCKDB.quote(what.schema),
            dbsql::DUCKDB.quote(what.relation)
        );
        what.sql_named(&dbsql::DUCKDB, &name)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            DuckSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        DuckSource::cancel(self);
        Ok(())
    }

    /// Statements run on the session connection, which is what a transaction
    /// needs in order to span two of them.
    ///
    /// True despite the three steps this database cannot take. The question is
    /// whether statements on this session can be wrapped in a transaction, and
    /// they can; savepoints are a step inside one, and `transaction` refuses
    /// those by name so that the gap is reported where it is rather than by
    /// hiding Commit and Rollback as well.
    ///
    /// Cancel reaches the statement, for the reason it does in the SQLite driver:
    /// the engine is in this process and interrupting it stops the work.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: true,
            cancel_stops_the_statement: true,
            switches_database: true,
        }
    }

    async fn transaction(&self, step: &TxStep) -> DbResult<()> {
        Ok(DuckSource::transaction(self, step).await?)
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
