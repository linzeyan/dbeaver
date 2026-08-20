//! `TrinoSource` seen through the `Driver` trait.
//!
//! A forward everywhere except two places, and both are about the level Trino
//! has that the trait does not: `browse` writes a three-part name, and
//! `transactional` is the one method here whose answer took a day of measuring
//! rather than a lookup.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, ColumnInfo, ConstraintInfo, Cursor as CursorApi, CursorCancel as CursorCancelApi,
    DbError, DbResult, Driver, IndexInfo, RelationInfo, RelationshipInfo, ResultStream, SchemaInfo,
    ServerInfo, TriggerInfo, TxStep, UniqueKeyInfo, scalar_text,
};

use crate::{Rows, RowsCancel, TrinoError, TrinoSource, parts};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error becomes a string.
impl From<TrinoError> for DbError {
    fn from(e: TrinoError) -> Self {
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
/// coordinator, but the trait method needs a `TrinoSource` and a `TrinoSource`
/// needs a server. A test that rebuilt the statement itself would be checking a
/// copy.
fn browse_sql(what: &Browse<'_>) -> String {
    let dialect = &dbsql::POSTGRES;
    let mut name = String::new();
    match parts(what.schema) {
        // The ordinary case: a schema that came from `schemas()` and therefore
        // holds both levels Trino has.
        Some((catalog, schema)) => {
            name.push_str(&dialect.quote(catalog));
            name.push('.');
            name.push_str(&dialect.quote(schema));
            name.push('.');
        }
        // A schema string with no catalog in it. Written as one level rather
        // than dropped, so that a browse composed by hand against a connection
        // whose URL named a catalog still resolves — the coordinator fills the
        // missing level in from `X-Trino-Catalog`.
        None if !what.schema.is_empty() => {
            name.push_str(&dialect.quote(what.schema));
            name.push('.');
        }
        None => {}
    }
    name.push_str(&dialect.quote(what.relation));
    what.sql_named(dialect, &name)
}

#[async_trait]
impl Driver for TrinoSource {
    /// Trino versions itself with a bare number — `435` — and that is the whole
    /// of what it reports.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new(
            "Trino",
            scalar_text(self, "SELECT version()").await?,
        ))
    }
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(TrinoSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(TrinoSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(TrinoSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(TrinoSource::definition(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: Trino has no indexes. See
    /// `metadata.rs`, where the grammar is quoted refusing `CREATE INDEX`.
    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(TrinoSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: Trino enforces no constraint of
    /// any kind, so nothing here names a row. See `metadata.rs`.
    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(TrinoSource::unique_keys(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: Trino declares no foreign keys,
    /// because it has no primary keys for one to reference.
    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(TrinoSource::foreign_keys(self, schema, relation).await?)
    }

    /// Empty for the same reason as `foreign_keys`, from the other end.
    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(TrinoSource::referenced_by(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: Trino has no constraint syntax
    /// and no constraint catalog. Nullability is on the column instead.
    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(TrinoSource::constraints(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: `CREATE TRIGGER` is a syntax
    /// error in Trino's grammar.
    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(TrinoSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            TrinoSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM "catalog"."schema"."table"`, in PostgreSQL's spelling,
    /// which Trino follows exactly — double quotes around an identifier, doubled
    /// inside it, and an unquoted name folded to lower case.
    ///
    /// The dialect is borrowed rather than added because a `dbsql` row is an
    /// editor's business and not a driver's: it decides where a token ends and
    /// which words are painted as keywords, and adding one would be a change to
    /// the highlighter dressed as a change to this crate. `dbsql::for_scheme`
    /// already documents PostgreSQL as the answer for a database with no row of
    /// its own, and here that answer happens to be right rather than a guess.
    ///
    /// The schema is split back into the two levels it was flattened from, and
    /// each is quoted on its own. That is one step further than the DuckDB
    /// driver goes with the same composite — it pastes the pair in as written —
    /// and the step is worth taking here: DuckDB's databases are named by the
    /// person who attached them, while a Trino catalog is named by a file on the
    /// coordinator that somebody else may well have called `Sales`, and an
    /// unquoted `Sales` resolves to `sales`. Quoting the pair as one identifier
    /// would be the other mistake: `"tpch.tiny"."orders"` asks for a catalog
    /// nobody has, and the coordinator says so — *Catalog must be specified when
    /// session catalog is not set*.
    fn browse(&self, what: &Browse<'_>) -> String {
        browse_sql(what)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            TrinoSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(TrinoSource::cancel(self).await?)
    }

    /// No — and Trino is a third case, which the trait's two sentences for this
    /// answer do not cover.
    ///
    /// It is not a database without transactions. `START TRANSACTION` works,
    /// interactively, over stateless HTTP: a client that sends
    /// `X-Trino-Transaction-Id: NONE` to declare it understands them gets an
    /// `X-Trino-Started-Transaction-Id` back, sends that id on everything
    /// afterwards, and `COMMIT` answers with `X-Trino-Clear-Transaction-Id`.
    /// Measured against Trino 483, including two statements running *at once*
    /// inside one transaction, which no connection-bound database here allows.
    /// Carrying that header is a dozen lines, and this driver does not carry it
    /// or any other session state — see the crate comment.
    ///
    /// It is also not the trait's second case, a driver whose statements do not
    /// share a connection. There is no connection to share and none is needed;
    /// the id is a header, so any number of statements can be inside the same
    /// transaction from any number of tasks.
    ///
    /// What stops it is that in a federating engine the question has no single
    /// answer. A Trino transaction is the coordinator's, and each connector
    /// decides for itself whether it will take a write inside one; a connector
    /// that declares single-statement writes refuses *every* write in a
    /// transaction with `AUTOCOMMIT_WRITE_CONFLICT` — "Catalog only supports
    /// writes using autocommit: memory" — and the refusal does not just fail the
    /// statement, it **aborts the transaction**, so everything after it answers
    /// `TRANSACTION_ALREADY_ABORTED` until somebody rolls back. `memory` is the
    /// only writable catalog a stock coordinator has, and it is one of those
    /// connectors. Worse, one Trino statement can touch two catalogs that
    /// disagree, so there is no moment at which this driver could know the
    /// answer even for the statement in front of it.
    ///
    /// A front end that offered Commit and Rollback here would be offering a
    /// mode whose first write breaks the session, on a database where nothing
    /// can tell you in advance whether it will. So the answer is no, and the
    /// integration suite pins the measurement — `a_write_inside_a_transaction_is
    /// _refused_by_the_only_writable_catalog` — so that a coordinator whose
    /// catalogs take writes in a transaction turns this into a failing test
    /// rather than a comment nobody rereads.
    fn transactional(&self) -> bool {
        false
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    ///
    /// Including the three savepoint steps, which Trino has no syntax for at all:
    /// `SAVEPOINT halfway` is a syntax error, and the parser lists every
    /// statement it does accept without one.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new(
            "this driver holds no transaction on a Trino session: the protocol has one, \
             but each connector decides whether it takes writes inside it, and the ones \
             that do not abort the whole transaction on the first attempt",
        ))
    }
}

/// `Rows` is both, because against Trino a cursor and a result stream are the
/// same object read the same way.
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

    use browse_sql as statement;

    /// The three-level name, which is the whole reason this driver writes the
    /// relation's name itself instead of calling `Browse::sql`.
    #[test]
    fn a_browse_names_the_catalog_the_schema_and_the_table() {
        let keys = ["orderkey".to_string()];
        assert_eq!(
            statement(&browse("tpch.tiny", None, None, &keys, None)),
            "SELECT * FROM tpch.tiny.orders ORDER BY orderkey"
        );
    }

    /// A catalog or a schema that is not already lower case has to keep its
    /// case, or the statement asks for a different one — which is the failure
    /// the DuckDB driver's raw paste would have here.
    #[test]
    fn a_catalog_that_is_not_lower_case_survives_being_written_down() {
        assert_eq!(
            statement(&browse("Sales.Public", None, None, &[], None)),
            r#"SELECT * FROM "Sales"."Public".orders"#
        );
    }

    /// A schema with a dot in it belongs to the schema, because a catalog cannot
    /// have one.
    #[test]
    fn a_dot_inside_a_schema_name_stays_inside_the_schema_name() {
        assert_eq!(
            statement(&browse("hive.year.2024", None, None, &[], None)),
            r#"SELECT * FROM hive."year.2024".orders"#
        );
    }

    /// The user's own filter and order reach the statement as typed, and the key
    /// columns are this side's and are quoted.
    #[test]
    fn the_users_order_comes_first_and_the_key_makes_it_total() {
        let keys = ["orderkey".to_string()];
        assert_eq!(
            statement(&browse(
                "tpch.tiny",
                Some("totalprice > 10"),
                Some("clerk desc"),
                &keys,
                None
            )),
            "SELECT * FROM tpch.tiny.orders WHERE totalprice > 10 \
             ORDER BY clerk desc, orderkey"
        );
    }

    /// A row ceiling is a `LIMIT` at the end, which is what Trino spells it as.
    #[test]
    fn a_row_ceiling_is_a_limit_at_the_end() {
        assert_eq!(
            statement(&browse("tpch.tiny", None, None, &[], Some(1000))),
            "SELECT * FROM tpch.tiny.orders LIMIT 1000"
        );
    }

    /// A schema string with no catalog in it is written as one level, and the
    /// coordinator fills the other in from the session — rather than producing
    /// `.orders`, which is not a relation anywhere.
    #[test]
    fn a_schema_with_no_catalog_still_produces_a_statement_that_runs() {
        assert_eq!(
            statement(&browse("tiny", None, None, &[], None)),
            "SELECT * FROM tiny.orders"
        );
        assert_eq!(
            statement(&browse("", None, None, &[], None)),
            "SELECT * FROM orders"
        );
    }
}
