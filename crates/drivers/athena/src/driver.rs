//! `AthenaSource` seen through the `Driver` trait.
//!
//! A forward everywhere except two places: `browse` writes a two-part name in
//! Presto's spelling, and `transactional` says no.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, TriggerInfo, TxStep,
    UniqueKeyInfo,
};

use crate::{AthenaError, AthenaSource, Rows, RowsCancel};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error becomes a string.
impl From<AthenaError> for DbError {
    fn from(e: AthenaError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

/// The statement a browse reads a relation with.
///
/// A free function rather than only the method body, so the tests below can
/// reach it: `browse` promises no I/O and there is nothing in here that needs an
/// account, but the trait method needs an `AthenaSource` and that needs
/// credentials. A test that rebuilt the statement itself would be checking a
/// copy.
///
/// The dialect is the standard's, reached for through `dbsql::POSTGRES` because
/// that is the row which spells a quoted identifier `"like this"` and a row
/// ceiling `LIMIT n` — which is exactly what Athena's engine does, it being
/// Presto. The Trino driver borrows the same row for the same reason and says
/// so at more length; the difference is that Trino's namespace has three levels
/// and this has two.
///
/// **The catalog is not in the name.** Athena carries it in
/// `QueryExecutionContext.Catalog`, which is where the API puts it, and a
/// three-part name is accepted only for some catalogs and some engine versions.
/// So the statement names `"database"."table"` and the request says which
/// catalog that database is in — which also means a browse statement pasted into
/// the AWS console resolves against whatever catalog is selected there, which is
/// the behaviour somebody pasting it would expect.
fn browse_sql(what: &Browse<'_>) -> String {
    let dialect = &dbsql::POSTGRES;
    let mut name = String::new();
    // A database with no name is not something `schemas()` produces, and
    // `.orders` is not a relation anywhere.
    if !what.schema.is_empty() {
        name.push_str(&dialect.quote(what.schema));
        name.push('.');
    }
    name.push_str(&dialect.quote(what.relation));
    what.sql_named(dialect, &name)
}

#[async_trait]
impl Driver for AthenaSource {
    /// The product, without a version. Athena runs an engine version Amazon
    /// chooses per workgroup rather than one the session can ask for, and no
    /// server has answered this driver to disagree.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new("Athena", ""))
    }
    /// A connection names one catalog and `schemas()` lists the databases in it,
    /// so the level above is already chosen by the connection string.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(AthenaSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(AthenaSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(AthenaSource::columns(self, schema, relation).await?)
    }

    /// Always `None`, and without a request: the catalog action does not carry a
    /// view's text and the statement that would is a query execution. See
    /// `metadata.rs`.
    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(AthenaSource::definition(self, schema, relation).await?)
    }

    /// Empty, always, and without a request: a Hive table has no index.
    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(AthenaSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always, and without a request: a Hive table is a set of files
    /// under a prefix, and nothing declares — let alone enforces — that a column
    /// of it holds a value once.
    async fn unique_keys(&self, _schema: &str, _relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(Vec::new())
    }

    /// Empty, always, and without a request: Hive declares no foreign keys,
    /// because it has no primary keys for one to reference.
    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(AthenaSource::foreign_keys(self, schema, relation).await?)
    }

    /// Empty for the same reason as `foreign_keys`, from the other end.
    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(AthenaSource::referenced_by(self, schema, relation).await?)
    }

    /// Empty, always, and without a request: Hive has no constraint syntax and
    /// Glue has no constraint catalog.
    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(AthenaSource::constraints(self, schema, relation).await?)
    }

    /// Empty, always, and without a request: Athena has no triggers.
    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(AthenaSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            AthenaSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM "database"."table"`, in Presto's spelling.
    fn browse(&self, what: &Browse<'_>) -> String {
        browse_sql(what)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            AthenaSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(AthenaSource::cancel(self).await?)
    }

    /// No, and this is the trait's first case rather than its second: Athena has
    /// no transactions to hold.
    ///
    /// There is no `BEGIN`, no `COMMIT` and no `ROLLBACK` in the grammar. What
    /// Athena has, on an Iceberg table, is atomicity *within* one statement — an
    /// `INSERT` either lands or does not — and nothing at all that spans two.
    /// The API agrees: a query execution is the unit, it is identified by an id
    /// the service chose, and there is no object anywhere in it that two
    /// executions could belong to.
    ///
    /// So this is not a driver that could do better with a connection held back.
    /// There is no connection, and if there were, there would still be nothing
    /// to say to it.
    ///
    /// Cancel reaches the statement: one `StopQueryExecution` per query in flight.
    ///
    /// Routines are not reported, and there are none to report. An Athena UDF is
    /// a Lambda function named in the statement that uses it — `USING EXTERNAL
    /// FUNCTION` — so it belongs to the query rather than to the catalog, and the
    /// Glue database this driver lists tables out of holds no such object.
    ///
    /// Sequences are not reported, and there is nothing to report. Athena reads
    /// files through a catalog of tables; a sequence would have to be held
    /// somewhere that writes, and nothing here does.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: true,
            switches_database: false,
            schema_is_the_database: true,
            reports_routines: false,
            reports_sequences: false,
        }
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    ///
    /// Including the three savepoint steps, which follow from the same absence:
    /// there is no transaction for a savepoint to be a point in.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new(
            "Athena has no transactions: there is no BEGIN in its grammar, and a query \
             execution is the largest thing it is atomic over",
        ))
    }
}

/// `Rows` is both, because against a finished query execution a cursor and a
/// result stream are the same object read the same way.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn browse<'a>(
        schema: &'a str,
        filter: Option<&'a str>,
        order: Option<&'a str>,
        keys: &'a [String],
        limit: Option<u32>,
    ) -> Browse<'a> {
        Browse {
            schema,
            relation: "orders",
            filter,
            order,
            keys,
            limit,
        }
    }

    /// Two levels and not three: the catalog rides in the request rather than in
    /// the statement.
    #[test]
    fn a_browse_names_the_database_and_the_table_and_not_the_catalog() {
        assert_eq!(
            browse_sql(&browse("sales", None, None, &[], None)),
            "SELECT * FROM sales.orders"
        );
    }

    /// A database name that is not already lower case has to keep its case, or
    /// the statement asks for a different one — Presto folds an unquoted name
    /// down.
    #[test]
    fn a_database_that_is_not_lower_case_survives_being_written_down() {
        assert_eq!(
            browse_sql(&browse("Sales", None, None, &[], None)),
            r#"SELECT * FROM "Sales".orders"#
        );
    }

    /// A browse composed against a connection that named no database still
    /// produces a statement that runs, rather than one with a leading dot.
    #[test]
    fn a_missing_database_is_left_out_rather_than_written_as_nothing() {
        assert_eq!(
            browse_sql(&browse("", None, None, &[], None)),
            "SELECT * FROM orders"
        );
    }

    /// The user's own filter and order reach the statement as typed; the key
    /// columns are this side's and are quoted. A row ceiling is a `LIMIT` at the
    /// end, which is where Presto puts it.
    #[test]
    fn the_users_order_comes_first_and_the_key_makes_it_total() {
        let keys = ["Id".to_string()];
        assert_eq!(
            browse_sql(&browse(
                "sales",
                Some("total > 10"),
                Some("clerk desc"),
                &keys,
                Some(1000)
            )),
            r#"SELECT * FROM sales.orders WHERE total > 10 ORDER BY clerk desc, "Id" LIMIT 1000"#
        );
    }
}
