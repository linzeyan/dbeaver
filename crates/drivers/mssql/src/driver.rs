//! `MsSqlSource` seen through the `Driver` trait.
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
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, ServerProcesses,
    TriggerInfo, TxStep, UniqueKeyInfo, scalar_text,
};

use crate::{ArrowStream, Cursor, CursorCancel, MsSqlError, MsSqlSource};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error is thrown away.
///
/// Both facts were settled further in, where the statement text and the memory
/// of having issued a `KILL` were still available. Recovering either by reading
/// the prose back is how a caret ends up pointing at whatever the message
/// happened to contain.
impl From<MsSqlError> for DbError {
    fn from(e: MsSqlError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

#[async_trait]
impl Driver for MsSqlSource {
    /// The version property rather than `@@VERSION`, which answers with four
    /// lines of banner: the edition, the build date and the operating system are
    /// prose around the one number this is asking for.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        let version = scalar_text(
            self,
            "SELECT CAST(SERVERPROPERTY('ProductVersion') AS varchar(128))",
        )
        .await?;
        Ok(ServerInfo::new("SQL Server", version))
    }
    /// The other driver here with a database level worth drawing.
    ///
    /// Built from this driver's own `databases()`, which already reads
    /// `sys.databases` and is already tested. That one carries a state and a
    /// collation the trait's level has no room for, so what crosses is the name
    /// and whether it is the one this session is on.
    ///
    /// Online ones only. A database that is restoring, offline or recovering is
    /// still in `sys.databases`, and offering one as somewhere to open would be
    /// offering something that cannot answer.
    ///
    /// SQL Server can change database within a session with `USE`. This does not:
    /// opening one of these means another connection, which is what the
    /// PostgreSQL driver has no choice about and what keeps one session meaning
    /// one database on both.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        let current = scalar_text(self, "SELECT DB_NAME()").await?;
        Ok(Some(
            MsSqlSource::databases(self)
                .await?
                .into_iter()
                .filter(|d| d.state == "ONLINE")
                .map(|d| DatabaseInfo {
                    is_current: d.name == current,
                    name: d.name,
                })
                .collect(),
        ))
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(MsSqlSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(MsSqlSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(MsSqlSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(MsSqlSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(MsSqlSource::indexes(self, schema, relation).await?)
    }

    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(MsSqlSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(MsSqlSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(MsSqlSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(MsSqlSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(MsSqlSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            MsSqlSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT TOP (n) * FROM …`, and quoted with brackets: `"…"` here means an
    /// identifier only while `QUOTED_IDENTIFIER` is on, and a statement whose
    /// meaning depends on a session setting is a statement this cannot promise.
    fn browse(&self, what: &Browse<'_>) -> String {
        what.sql(&dbsql::MSSQL)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            MsSqlSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(MsSqlSource::cancel(self).await?)
    }

    /// Statements share one connection here, which is what a transaction needs
    /// in order to span them.
    ///
    /// Cancel reaches the statement: `KILL` aimed at the connections that are
    /// actually busy.
    ///
    /// A database is not somewhere this session can move to, and this is the one
    /// place in that answer where the database is not the reason. T-SQL has
    /// `USE`, and it would move the connection statements run on and leave the
    /// pool the catalog is read through where it was — a navigator listing one
    /// database's tables while the editor was writing to another. Reporting
    /// false is what sends the front end down the path that moves all of it: a
    /// new connection with the other name in the string.
    ///
    /// Routines are not reported, and that is a gap — the largest of the ones
    /// this field records, because SQL Server is where a reader most expects the
    /// group. `sys.objects` marks the four kinds it has and `sys.sql_modules`
    /// holds the source of each. Nothing here reads either yet.
    ///
    /// Sequences are not reported, and that is a gap: `sys.sequences` has held
    /// them since 2012, with every column the pane would show. Unwritten rather
    /// than absent.
    ///
    /// The server's activity is not reported, and that is a gap rather than an
    /// absence. `sys.dm_exec_sessions` and `sys.dm_exec_requests` are the list
    /// and `KILL` takes a session id, so one of the two verbs is there to
    /// read — this driver has been taught neither.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: true,
            cancel_stops_the_statement: true,
            switches_database: false,
            schema_is_the_database: false,
            reports_routines: false,
            reports_sequences: false,
            server_processes: ServerProcesses::Unreported,
        }
    }

    async fn transaction(&self, step: &TxStep) -> DbResult<()> {
        Ok(MsSqlSource::transaction(self, step).await?)
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
