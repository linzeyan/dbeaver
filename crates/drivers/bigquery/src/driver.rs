//! `BigQuerySource` seen through the `Driver` trait.
//!
//! A forward everywhere except two places: `browse` writes a three-part name,
//! and `transactional` says no about a database that does have transactions.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, TriggerInfo, TxStep,
    UniqueKeyInfo,
};

use crate::{BigQueryError, BigQuerySource, Rows, RowsCancel};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error becomes a string.
impl From<BigQueryError> for DbError {
    fn from(e: BigQueryError) -> Self {
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
/// reach it: `browse` promises no I/O and there is nothing in here that needs a
/// project, but the trait method needs a `BigQuerySource` and that needs
/// credentials. A test that rebuilt the statement itself would be checking a
/// copy.
///
/// The dialect is MySQL's, which is an uncomfortable name for the right row.
/// GoogleSQL quotes an identifier with backticks and reads `"abc"` as a *string*
/// — both of those are exactly what the `MYSQL` row says, and they are the two
/// facts this statement depends on. What comes with them and does not fit is
/// MySQL's keyword list, which decides only whether a name gets quoted that
/// need not be: a BigQuery reserved word missing from that list is written
/// unquoted and refused by the server, which is visible, and a MySQL keyword
/// that BigQuery does not reserve is quoted unnecessarily, which is harmless. A
/// row of GoogleSQL's own was considered and left alone, for the reason the
/// Flight SQL driver gives: a `dbsql` row is an editor's business — it decides
/// where a token ends and which words are painted — and adding one would be a
/// change to the highlighter dressed as a change to this crate.
///
/// The backtick-doubling `dbsql` applies inside a quoted name is never
/// exercised here, because a BigQuery identifier cannot contain a backtick. A
/// name that did would not be a BigQuery name.
fn browse_sql(project: &str, what: &Browse<'_>) -> String {
    let dialect = &dbsql::MYSQL;
    // Each level quoted on its own. Quoting the composite instead —
    // `` `project.dataset.table` `` — is also valid GoogleSQL and is what the
    // console prints, and it is avoided for the reason the Flight SQL driver
    // avoids it with its own two levels: a dataset id cannot contain a dot, but
    // treating the whole path as one name means a project holding one would
    // silently address something else. Quoting the parts keeps each name meaning
    // exactly what the catalog says it means.
    //
    // The project is written in even though the job already runs in it, because
    // this statement is shown to the person about to run it and may be pasted
    // into a console whose default project is a different one. A hyphenated
    // project id — which is most of them — is backticked by the dialect, since
    // unquoted it would be read as subtraction.
    let mut name = String::new();
    for part in [project, what.schema].iter().filter(|p| !p.is_empty()) {
        name.push_str(&dialect.quote(part));
        name.push('.');
    }
    name.push_str(&dialect.quote(what.relation));
    what.sql_named(dialect, &name)
}

#[async_trait]
impl Driver for BigQuerySource {
    /// BigQuery has no version to report — it is a service rather than a server
    /// somebody runs a build of — so the product is the whole of the answer.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new("BigQuery", ""))
    }
    /// A connection names one project and `schemas()` lists its datasets. The
    /// level above a dataset is the project, and it is a connection-string
    /// question here rather than something to browse.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(BigQuerySource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(BigQuerySource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(BigQuerySource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(BigQuerySource::definition(self, schema, relation).await?)
    }

    /// Empty, always, and without a request: BigQuery has no index of the kind
    /// this field describes. See `metadata.rs`.
    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(BigQuerySource::indexes(self, schema, relation).await?)
    }

    /// Empty, always, and without a request. BigQuery's primary and foreign keys
    /// are declared `NOT ENFORCED` and there is no unique constraint at all, so
    /// there is nothing here a row could be named by that the warehouse promises
    /// is unique.
    async fn unique_keys(&self, _schema: &str, _relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(Vec::new())
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(BigQuerySource::foreign_keys(self, schema, relation).await?)
    }

    /// Empty, always, and without a request — the one metadata answer here that
    /// is a cost decision rather than a fact about BigQuery. See `metadata.rs`.
    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(BigQuerySource::referenced_by(self, schema, relation).await?)
    }

    /// Empty, always, and without a request: BigQuery has no check, unique or
    /// exclusion constraint.
    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(BigQuerySource::constraints(self, schema, relation).await?)
    }

    /// Empty, always, and without a request: GoogleSQL has no `CREATE TRIGGER`.
    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(BigQuerySource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            BigQuerySource::query(self, statement, batch_rows).await?,
        ))
    }

    /// ``SELECT * FROM `project`.`dataset`.`table` ``, in GoogleSQL's spelling.
    fn browse(&self, what: &Browse<'_>) -> String {
        browse_sql(self.project(), what)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            BigQuerySource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(BigQuerySource::cancel(self).await?)
    }

    /// No — and not because BigQuery has no transactions.
    ///
    /// It has them. `BEGIN TRANSACTION` … `COMMIT` is real GoogleSQL, it holds
    /// across statements, and it rolls back. What it holds across is a
    /// **script**: one job containing several statements, submitted together and
    /// planned together. There is no session for a transaction to live in,
    /// because there is no session — every statement this driver runs is its own
    /// job, and two jobs are two independent units of work whatever order they
    /// were submitted in.
    ///
    /// So this is not the trait's first case, a database without transactions,
    /// and it is not quite the second either — there is no connection to share
    /// because BigQuery has none to offer. It is a third: the unit the database
    /// scopes a transaction to is smaller than the unit a client submits work
    /// in, and no arrangement inside a driver changes that.
    ///
    /// The way to offer Commit and Rollback anyway would be to stop submitting
    /// statements when `Begin` arrives, hold them in a list, and send the list as
    /// one script when `Commit` does. That is a client pretending to be a
    /// session: every statement in the transaction would report its results at
    /// commit time rather than when it was run, a `SELECT` inside one would
    /// return nothing until the user committed, and an editor's Cancel button
    /// would have nothing to cancel. A front end that hides the two buttons is
    /// telling the truth; one that offered them would not be.
    ///
    /// Cancel reaches the statement, in whichever of its two states it is in: a
    /// job still running is stopped by `jobs.cancel`, and rows still being read
    /// are stopped on this side.
    ///
    /// Routines are not reported, and that is a gap rather than a fact about
    /// BigQuery: a dataset can hold user-defined functions, table functions and
    /// procedures, and `INFORMATION_SCHEMA.ROUTINES` lists them. Nothing here
    /// reads it yet.
    ///
    /// Sequences are not reported, and BigQuery has none. A generated key is a
    /// value an INSERT computes; nothing in a dataset holds a counter.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: true,
            switches_database: false,
            schema_is_the_database: false,
            reports_routines: false,
            reports_sequences: false,
        }
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    ///
    /// Including the three savepoint steps. GoogleSQL has no `SAVEPOINT` at all
    /// — not inside a script either — so those are refused for a second reason
    /// on top of the first.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new(
            "this driver holds no transaction on a BigQuery connection: BigQuery scopes one \
             to a script, which is a single job, and every statement here is a job of its own",
        ))
    }
}

/// `Rows` is both, because against a read session a cursor and a result stream
/// are the same object read the same way.
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

    /// The three-level name, and the reason it is written with backticks: a
    /// project id holds hyphens, and unquoted a hyphen is subtraction.
    #[test]
    fn a_browse_names_the_project_the_dataset_and_the_table() {
        assert_eq!(
            browse_sql("my-project-123", &browse("sales", None, None, &[], None)),
            "SELECT * FROM `my-project-123`.sales.orders"
        );
        // A project id that needs no quoting does not get any, because a browse
        // statement is shown to the person about to run it.
        assert_eq!(
            browse_sql("project", &browse("sales", None, None, &[], None)),
            "SELECT * FROM project.sales.orders"
        );
    }

    /// A dataset name that is not already lower case has to keep its case, or
    /// the statement asks for a different dataset — BigQuery's dataset ids are
    /// case-sensitive.
    #[test]
    fn a_dataset_that_is_not_lower_case_survives_being_written_down() {
        assert_eq!(
            browse_sql("p", &browse("Sales", None, None, &[], None)),
            "SELECT * FROM p.`Sales`.orders"
        );
    }

    /// A browse composed against a connection that named no dataset still
    /// produces a statement that runs, rather than one with an empty level in
    /// it.
    #[test]
    fn a_missing_level_is_left_out_rather_than_written_as_nothing() {
        assert_eq!(
            browse_sql("p", &browse("", None, None, &[], None)),
            "SELECT * FROM p.orders"
        );
        assert_eq!(
            browse_sql("", &browse("", None, None, &[], None)),
            "SELECT * FROM orders"
        );
    }

    /// The user's own filter and order reach the statement as typed; the key
    /// columns are this side's and are quoted. A row ceiling is a `LIMIT` at the
    /// end, which is where GoogleSQL puts it.
    #[test]
    fn the_users_order_comes_first_and_the_key_makes_it_total() {
        let keys = ["Id".to_string()];
        assert_eq!(
            browse_sql(
                "p",
                &browse(
                    "sales",
                    Some("total > 10"),
                    Some("clerk desc"),
                    &keys,
                    Some(1000)
                )
            ),
            "SELECT * FROM p.sales.orders WHERE total > 10 \
             ORDER BY clerk desc, `Id` LIMIT 1000"
        );
    }
}
