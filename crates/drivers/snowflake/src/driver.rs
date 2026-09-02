//! `SnowflakeSource` seen through the `Driver` trait.
//!
//! A forward everywhere except two places: `browse` writes a three-part name and
//! quotes every part of it, and `transactional` is false for a reason that is
//! about the API rather than about the database.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, ServerProcesses,
    TriggerInfo, TxStep, UniqueKeyInfo, scalar_text,
};

use crate::{Rows, RowsCancel, SnowflakeError, SnowflakeSource, parts, quote};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error becomes a string.
///
/// Two of the three, in fact: no position. The SQL API's failures carry no field
/// for one — Snowflake writes `at position 7` into the message text instead — and
/// parsing a sentence this driver has never seen an example of would be a caret
/// placed by guesswork. Reporting none is the honest answer, and it is the same
/// one the Flight SQL driver gives for the same reason.
impl From<SnowflakeError> for DbError {
    fn from(e: SnowflakeError) -> Self {
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string()).as_cancelled(cancelled)
    }
}

/// The statement a browse reads a relation with.
///
/// A free function rather than only the method body, so the tests below can
/// reach it: `browse` promises no I/O and there is nothing in here that needs an
/// account, but the trait method needs a `SnowflakeSource` and a
/// `SnowflakeSource` needs a server. A test that rebuilt the statement itself
/// would be checking a copy.
///
/// `Browse::sql_named` is deliberately not reached for, and it is the only SQL
/// driver here that declines it. That helper quotes a name only when it is not
/// already lower case, which is right for every database that folds an unquoted
/// identifier *down* and exactly backwards for Snowflake, which folds up: a
/// column the catalog calls `orders` would be written bare and resolve to
/// `ORDERS`. So every identifier below goes through `crate::quote`, which always
/// quotes, and the filter and the order stay the user's own words.
fn browse_sql(what: &Browse<'_>) -> String {
    let mut name = String::new();
    match parts(what.schema) {
        // The ordinary case: a schema that came from `schemas()` and therefore
        // holds both levels Snowflake has.
        Some((database, schema)) => {
            name.push_str(&quote(database));
            name.push('.');
            name.push_str(&quote(schema));
            name.push('.');
        }
        // A schema string with no database in it. Written as one level rather
        // than dropped, so that a browse composed by hand against a connection
        // whose URL named a database still resolves — the account fills the
        // missing level in from the request.
        None if !what.schema.is_empty() => {
            name.push_str(&quote(what.schema));
            name.push('.');
        }
        None => {}
    }
    name.push_str(&quote(what.relation));

    let mut sql = format!("SELECT * FROM {name}");
    if let Some(filter) = what.filter.map(str::trim).filter(|f| !f.is_empty()) {
        // As typed. The filter bar takes an expression in the database's own
        // language, and a client that rewrote it would be parsing SQL in order
        // to hand it back.
        sql.push_str(" WHERE ");
        sql.push_str(filter);
    }
    let mut terms = Vec::new();
    if let Some(order) = what.order.map(str::trim).filter(|o| !o.is_empty()) {
        terms.push(order.to_string());
    }
    terms.extend(what.keys.iter().map(|key| quote(key)));
    if !terms.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&terms.join(", "));
    }
    if let Some(rows) = what.limit {
        sql.push_str(&format!(" LIMIT {rows}"));
    }
    sql
}

#[async_trait]
impl Driver for SnowflakeSource {
    /// The one driver here whose version is asked for without a server ever
    /// having answered: `CURRENT_VERSION()` is Snowflake's own documented way to
    /// state it, so it is written rather than left blank — a statement from the
    /// vendor's documentation is a different thing from a guess.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new(
            "Snowflake",
            scalar_text(self, "SELECT CURRENT_VERSION()").await?,
        ))
    }
    /// Two levels already flattened: a Snowflake database is carried in the
    /// `SchemaInfo` name rather than drawn above it.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(SnowflakeSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(SnowflakeSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(SnowflakeSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(SnowflakeSource::definition(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: Snowflake has no indexes. See
    /// `metadata.rs`.
    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(SnowflakeSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always — and this is the one place where having the answer and not
    /// reporting it is the correct move.
    ///
    /// `SHOW UNIQUE KEYS` works, and `constraints` above already shows what it
    /// returns in the structure pane, where a declaration is worth reading.
    /// Naming a row by one is different: Snowflake enforces no constraint except
    /// `NOT NULL`, so a UNIQUE constraint there is a statement of intent and not
    /// a guarantee, and a table can hold two rows that satisfy it. An UPDATE
    /// built from one would silently change both.
    async fn unique_keys(&self, _schema: &str, _relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(Vec::new())
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(SnowflakeSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(SnowflakeSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(SnowflakeSource::constraints(self, schema, relation).await?)
    }

    /// Empty, always, and without a statement: Snowflake has no triggers.
    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(SnowflakeSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            SnowflakeSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM "DATABASE"."SCHEMA"."TABLE"`, with every part quoted.
    fn browse(&self, what: &Browse<'_>) -> String {
        browse_sql(what)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            SnowflakeSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(SnowflakeSource::cancel(self).await?)
    }

    /// No — and the reason is the API rather than the database.
    ///
    /// Snowflake has ordinary transactions: `BEGIN`, `COMMIT`, `ROLLBACK`, with
    /// read-committed isolation, and every JDBC client here uses them. What it
    /// does not have is a session over the SQL API. Each `POST /api/v2
    /// /statements` stands alone, carries its own database, schema, warehouse and
    /// role, and is executed in a transaction of its own; there is nothing to
    /// send a second statement *down*, so a `BEGIN` in one request opens a
    /// transaction the next request cannot reach and the account closes when the
    /// implicit session ends.
    ///
    /// That is the trait's second case — a driver whose statements do not share
    /// a connection — rather than its first, and the difference matters: the
    /// database is not missing anything, this transport is. A front end that
    /// offered Commit and Rollback here would be offering buttons whose effect is
    /// nothing at all, which is worse than a database that says it has no
    /// transactions.
    ///
    /// Snowflake has no savepoints under any transport, so the three savepoint
    /// steps have nowhere to go regardless.
    ///
    /// Cancel reaches the statement: one cancel per statement in flight.
    ///
    /// Routines are not reported, and that is a gap: a Snowflake schema can hold
    /// functions and procedures, and `information_schema.functions` and
    /// `information_schema.procedures` list them separately. Nothing here reads
    /// either yet.
    ///
    /// Sequences are not reported, and that is a gap: a Snowflake schema can
    /// hold them and `information_schema.sequences` lists them. Nothing here
    /// reads it yet.
    ///
    /// The server's activity is not reported, and that is a gap. Snowflake lists
    /// running statements through its query history and stops one with
    /// `SYSTEM$CANCEL_QUERY`; this driver asks for neither.
    ///
    /// The server's settings are not listed, and that is a gap. `SHOW
    /// PARAMETERS` returns them with the level each was set at — account, user
    /// or session — which is the distinction `VariableScope` draws; this driver
    /// does not read it yet.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: true,
            switches_database: false,
            schema_is_the_database: false,
            reports_routines: false,
            reports_sequences: false,
            server_processes: ServerProcesses::Unreported,
            reports_variables: false,
            // Its edits are SQL, composed above this driver from the dialect
            // this build carries for it.
            writes_rows: false,
        }
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new(
            "this driver holds no transaction on a Snowflake session: the SQL API has no \
             session for one to live in, so each statement is its own transaction and a \
             BEGIN sent here would be reachable by nothing that follows it",
        ))
    }
}

/// `Rows` is both, because against the SQL API a cursor and a result stream are
/// the same object read the same way.
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
            relation: "ORDERS",
            filter,
            order,
            keys,
            limit,
        }
    }

    use browse_sql as statement;

    /// The three-level name, and the quoting that makes it mean what the catalog
    /// said. This is the test that would fail if somebody replaced this function
    /// with `Browse::sql_named`.
    #[test]
    fn a_browse_names_the_database_the_schema_and_the_table_and_quotes_all_three() {
        let keys = ["O_ORDERKEY".to_string()];
        assert_eq!(
            statement(&browse("SALES.PUBLIC", None, None, &keys, None)),
            r#"SELECT * FROM "SALES"."PUBLIC"."ORDERS" ORDER BY "O_ORDERKEY""#
        );
    }

    /// A lower-case name is the case that separates Snowflake from every other
    /// SQL database here: written bare it would fold *up* and name a relation
    /// that is not the one the navigator was showing.
    #[test]
    fn a_lower_case_name_is_quoted_because_snowflake_folds_the_other_way() {
        let what = Browse {
            schema: "sales.public",
            relation: "orders",
            filter: None,
            order: None,
            keys: &[],
            limit: None,
        };
        assert_eq!(
            statement(&what),
            r#"SELECT * FROM "sales"."public"."orders""#
        );
    }

    /// A schema with a dot in it belongs to the schema, because the database is
    /// the half that is named first.
    #[test]
    fn a_dot_inside_a_schema_name_stays_inside_the_schema_name() {
        assert_eq!(
            statement(&browse("SALES.YEAR.2024", None, None, &[], None)),
            r#"SELECT * FROM "SALES"."YEAR.2024"."ORDERS""#
        );
    }

    /// The user's own filter and order reach the statement as typed, and the key
    /// columns are this side's and are quoted.
    #[test]
    fn the_users_order_comes_first_and_the_key_makes_it_total() {
        let keys = ["O_ORDERKEY".to_string()];
        assert_eq!(
            statement(&browse(
                "SALES.PUBLIC",
                Some("O_TOTALPRICE > 10"),
                Some("O_CLERK desc"),
                &keys,
                None
            )),
            concat!(
                r#"SELECT * FROM "SALES"."PUBLIC"."ORDERS" WHERE O_TOTALPRICE > 10 "#,
                r#"ORDER BY O_CLERK desc, "O_ORDERKEY""#
            )
        );
    }

    /// A row ceiling is a `LIMIT` at the end, which is what Snowflake spells it
    /// as.
    #[test]
    fn a_row_ceiling_is_a_limit_at_the_end() {
        assert_eq!(
            statement(&browse("SALES.PUBLIC", None, None, &[], Some(1000))),
            r#"SELECT * FROM "SALES"."PUBLIC"."ORDERS" LIMIT 1000"#
        );
    }

    /// A schema string with no database in it is written as one level, and the
    /// account fills the other in from the request — rather than producing
    /// `."ORDERS"`, which is not a relation anywhere.
    #[test]
    fn a_schema_with_no_database_still_produces_a_statement_that_runs() {
        assert_eq!(
            statement(&browse("PUBLIC", None, None, &[], None)),
            r#"SELECT * FROM "PUBLIC"."ORDERS""#
        );
        assert_eq!(
            statement(&browse("", None, None, &[], None)),
            r#"SELECT * FROM "ORDERS""#
        );
    }
}
