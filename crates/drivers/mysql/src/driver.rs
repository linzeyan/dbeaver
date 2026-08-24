//! `MySqlSource` seen through the `Driver` trait.
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
    /// `SELECT VERSION()`, read for the product as well as for the number.
    ///
    /// MySQL answers with a bare version — `8.0.35` — and the products speaking
    /// its protocol append their own name to one: `8.0.11-TiDB-v7.5.0`,
    /// `5.5.5-10.6.4-MariaDB`. So the string names the product where there is one
    /// to name, and the version is kept whole, because the compatibility version
    /// in front of it is part of what the server said.
    ///
    /// StarRocks is not in the list because it cannot be: it answers with a
    /// MySQL version and nothing of its own, and a guess here would be printed to
    /// somebody as a fact.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        let version = scalar_text(self, "SELECT VERSION()").await?;
        let product = if version.contains("TiDB") {
            "TiDB"
        } else if version.contains("MariaDB") {
            "MariaDB"
        } else {
            "MySQL"
        };
        Ok(ServerInfo::new(product, version))
    }
    /// A schema and a database are the same object here, so `schemas()` is
    /// already the list of databases. A level above it would be the same names
    /// twice.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(MySqlSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(MySqlSource::relations(self, schema).await?)
    }

    async fn routines(&self, schema: &str) -> DbResult<Vec<RoutineInfo>> {
        Ok(MySqlSource::routines(self, schema).await?)
    }

    async fn routine_definition(&self, schema: &str, id: &str) -> DbResult<Option<String>> {
        Ok(MySqlSource::routine_definition(self, schema, id).await?)
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

    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(MySqlSource::unique_keys(self, schema, relation).await?)
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

    /// `SELECT * FROM …`, quoted with backticks rather than the standard's
    /// double quotes: without `ANSI_QUOTES` in `sql_mode`, which is off by
    /// default, MySQL reads `"orders"` as the string "orders" and the statement
    /// selects from nothing.
    fn browse(&self, what: &Browse<'_>) -> String {
        what.sql(&dbsql::MYSQL)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            MySqlSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(MySqlSource::cancel(self).await?)
    }

    /// Statements run on the session connection here, which is what a
    /// transaction needs to span them — where the server has one to span.
    ///
    /// The only driver here where that second clause is not rhetorical. The same
    /// code reaches MySQL, TiDB and StarRocks, so this cannot be a constant:
    /// StarRocks is a distributed column store whose transactions stop at
    /// `BEGIN` and `COMMIT`, and which of the three is on the other end of the
    /// socket is asked at connect rather than assumed. `metadata::probe` has the
    /// evidence and the reasoning.
    ///
    /// This is also the driver that makes `Capabilities` a question for the open
    /// session rather than for the scheme: the answer above is read off what
    /// answered, and StarRocks and Doris arrive down the same wire as MySQL.
    ///
    /// Cancel reaches the statement — `KILL QUERY` naming the connection it is
    /// running on.
    ///
    /// Routines are reported. `information_schema.ROUTINES` is one of the tables
    /// every server down this wire implements, including the two that are not
    /// MySQL: StarRocks and Doris answer it, with no rows, which is the right
    /// answer for engines that have no `CREATE FUNCTION` of their own.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: MySqlSource::transactional(self),
            cancel_stops_the_statement: true,
            switches_database: false,
            schema_is_the_database: true,
            reports_routines: true,
        }
    }

    async fn transaction(&self, step: &TxStep) -> DbResult<()> {
        Ok(MySqlSource::transaction(self, step).await?)
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
