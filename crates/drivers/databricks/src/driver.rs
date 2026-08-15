//! `DatabricksSource` seen through the `Driver` trait.
//!
//! A forward everywhere except two places: `browse` writes a three-part name, and
//! `transactional` is false for a reason that is about the database rather than
//! about the transport — which makes it the opposite case from the Snowflake
//! driver, whose answer is the same and whose reason is not.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, ColumnInfo, ConstraintInfo, Cursor as CursorApi, CursorCancel as CursorCancelApi,
    DbError, DbResult, Driver, IndexInfo, RelationInfo, RelationshipInfo, ResultStream, SchemaInfo,
    TriggerInfo, TxStep,
};

use crate::{DatabricksError, DatabricksSource, Rows, RowsCancel, parts};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error becomes a string.
///
/// Two of the three: no position. The API's failure carries an error code and a
/// message and no offset into the statement, and Databricks writes the position
/// into the message text — `[PARSE_SYNTAX_ERROR] Syntax error at or near 'FROM'`,
/// sometimes with a line and column in a second sentence. Parsing prose this
/// driver has never seen an example of would be a caret placed by guesswork, so
/// none is reported. The Flight SQL driver declines the same thing for the same
/// reason.
impl From<DatabricksError> for DbError {
    fn from(e: DatabricksError) -> Self {
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string()).as_cancelled(cancelled)
    }
}

/// The statement a browse reads a relation with.
///
/// A free function rather than only the method body, so the tests below can
/// reach it: `browse` promises no I/O and there is nothing in here that needs a
/// warehouse, but the trait method needs a `DatabricksSource` and a
/// `DatabricksSource` needs a server. A test that rebuilt the statement itself
/// would be checking a copy.
///
/// The dialect is MySQL's, borrowed for the two things a browse needs from one:
/// backtick identifiers and a trailing `LIMIT`, which is how Databricks SQL
/// spells both. Its double quote is a string delimiter in both, which is the
/// third agreement and the reason the borrow is not a coincidence — Spark SQL
/// took Hive's quoting and Hive took MySQL's.
///
/// Borrowing rather than adding a row to `dbsql` is the Trino driver's argument,
/// unchanged: a dialect there decides where a token ends and which words are
/// painted as keywords, so adding one would be a change to the editor's
/// highlighter dressed as a change to this crate. The visible consequence is that
/// the editor paints Databricks SQL as PostgreSQL, which `dbsql::for_scheme`
/// documents as what an unrecognised scheme gets, while the statement this
/// function writes is in Databricks' own spelling. A wrong guess there costs
/// colour; a wrong guess here would cost a syntax error.
fn browse_sql(what: &Browse<'_>) -> String {
    let dialect = &dbsql::MYSQL;
    let mut name = String::new();
    match parts(what.schema) {
        // The ordinary case: a schema that came from `schemas()` and therefore
        // holds both levels Unity Catalog has.
        Some((catalog, schema)) => {
            name.push_str(&dialect.quote(catalog));
            name.push('.');
            name.push_str(&dialect.quote(schema));
            name.push('.');
        }
        // A schema string with no catalog in it. Written as one level rather than
        // dropped, so that a browse composed by hand against a connection whose
        // URL named a catalog still resolves — the warehouse fills the missing
        // level in from the request.
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
impl Driver for DatabricksSource {
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(DatabricksSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(DatabricksSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(DatabricksSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(DatabricksSource::definition(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: a Delta table has no indexes. See
    /// `metadata.rs`.
    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(DatabricksSource::indexes(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(DatabricksSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(DatabricksSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(DatabricksSource::constraints(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: Databricks has no triggers.
    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(DatabricksSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            DatabricksSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// ``SELECT * FROM `catalog`.`schema`.`table` ``, in Databricks' own
    /// spelling.
    fn browse(&self, what: &Browse<'_>) -> String {
        browse_sql(what)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            DatabricksSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(DatabricksSource::cancel(self).await?)
    }

    /// No, and this is the trait's *first* case rather than its second: the
    /// database has no transactions to hold.
    ///
    /// Databricks SQL has no `BEGIN`, no `COMMIT` and no `SAVEPOINT` — they are
    /// not in the grammar. What Delta Lake gives instead is atomicity per
    /// statement: one `INSERT` or `MERGE` is one version of the table, all of it
    /// or none, and there is no way to put two statements inside one version from
    /// a client. So this is not the Snowflake situation, where the database has
    /// transactions and the SQL API has no session to hold one in; there is
    /// nothing here for a session to hold.
    ///
    /// Worth stating because the two drivers answer the same way and a reader
    /// comparing them should not conclude that REST is what costs a transaction.
    /// It costs Snowflake one. It costs Databricks nothing, because there was
    /// none.
    fn transactional(&self) -> bool {
        false
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new(
            "Databricks SQL has no transactions to hold: BEGIN, COMMIT and SAVEPOINT are not \
             in its grammar, and Delta Lake makes each statement atomic on its own instead",
        ))
    }
}

/// `Rows` is both, because against this API a cursor and a result stream are the
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
    ///
    /// `default` is quoted because it is a reserved word, and that is not an
    /// incidental detail of this test: every Unity Catalog catalog has a schema
    /// called `default`, so the most ordinary browse there is happens to be the
    /// case a dialect without keyword handling would turn into a syntax error.
    #[test]
    fn a_browse_names_the_catalog_the_schema_and_the_table() {
        let keys = ["o_orderkey".to_string()];
        assert_eq!(
            statement(&browse("main.default", None, None, &keys, None)),
            "SELECT * FROM main.`default`.orders ORDER BY o_orderkey"
        );
    }

    /// A name that is not already lower case is quoted, and the quotes are
    /// backticks — which is the one thing a browse would get wrong if it borrowed
    /// PostgreSQL's dialect as the Trino driver does. Databricks reads `"Sales"`
    /// as a string, so the statement would compare a column to a constant instead
    /// of naming a catalog.
    #[test]
    fn a_name_that_needs_quoting_gets_backticks_and_not_double_quotes() {
        let sql = statement(&browse("Sales.Public", None, None, &[], None));
        assert_eq!(sql, "SELECT * FROM `Sales`.`Public`.orders");
        assert!(!sql.contains('"'), "{sql}");
    }

    /// A schema with a dot in it belongs to the schema, because the catalog is
    /// the half that is named first.
    #[test]
    fn a_dot_inside_a_schema_name_stays_inside_the_schema_name() {
        assert_eq!(
            statement(&browse("main.year.2024", None, None, &[], None)),
            "SELECT * FROM main.`year.2024`.orders"
        );
    }

    /// The user's own filter and order reach the statement as typed, and the key
    /// columns are this side's and are quoted.
    #[test]
    fn the_users_order_comes_first_and_the_key_makes_it_total() {
        let keys = ["o_orderkey".to_string()];
        assert_eq!(
            statement(&browse(
                "main.default",
                Some("o_totalprice > 10"),
                Some("o_clerk desc"),
                &keys,
                None
            )),
            concat!(
                "SELECT * FROM main.`default`.orders WHERE o_totalprice > 10 ",
                "ORDER BY o_clerk desc, o_orderkey"
            )
        );
    }

    /// A row ceiling is a `LIMIT` at the end, which is what Databricks spells it
    /// as.
    #[test]
    fn a_row_ceiling_is_a_limit_at_the_end() {
        assert_eq!(
            statement(&browse("main.default", None, None, &[], Some(1000))),
            "SELECT * FROM main.`default`.orders LIMIT 1000"
        );
    }

    /// A schema string with no catalog in it is written as one level, and the
    /// warehouse fills the other in from the request — rather than producing
    /// `.orders`, which is not a relation anywhere.
    #[test]
    fn a_schema_with_no_catalog_still_produces_a_statement_that_runs() {
        assert_eq!(
            statement(&browse("default", None, None, &[], None)),
            "SELECT * FROM `default`.orders"
        );
        assert_eq!(
            statement(&browse("", None, None, &[], None)),
            "SELECT * FROM orders"
        );
    }
}
