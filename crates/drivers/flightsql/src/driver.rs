//! `FlightSqlSource` seen through the `Driver` trait.
//!
//! A thin layer on purpose, as everywhere else. The one method with anything to
//! decide is `browse`, and what it decides is which SQL to write for a server
//! that does not say what SQL it speaks.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, TriggerInfo, TxStep,
    UniqueKeyInfo,
};

use crate::{FlightSqlError, FlightSqlSource, Rows, RowsCancel};

/// The three questions a front end asks of a failure.
///
/// The position is always `None`, and that is a decision rather than an omission:
/// see the crate comment. What is behind a Flight SQL server is not knowable from
/// the protocol, so the only place a caret could come from is the engine's prose.
impl From<FlightSqlError> for DbError {
    fn from(e: FlightSqlError) -> Self {
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string()).as_cancelled(cancelled)
    }
}

/// The statement a browse reads a relation with.
///
/// A free function rather than only the method body, so the tests below can reach
/// it: `browse` promises no I/O and there is nothing in here that needs a session,
/// but the trait method needs a `FlightSqlSource` and that needs a server. A test
/// that rebuilt the statement itself would be checking a copy.
///
/// The dialect is the standard's, reached for through `dbsql::POSTGRES` because
/// that is the row in that table which spells a quoted identifier `"like this"`
/// and a row ceiling `LIMIT n` — the two things this statement needs. Naming a
/// dialect at all is uncomfortable here and the discomfort is the point: a Flight
/// SQL server does not say what engine is behind it, so any statement this driver
/// writes is a bet. The standard spelling is the one most of them take — Dremio,
/// DataFusion, DuckDB and Snowflake all read `"` as an identifier quote — and the
/// bet is placed in one function where it can be seen.
fn browse_sql(what: &Browse<'_>) -> String {
    let dialect = &dbsql::POSTGRES;
    // Each level quoted separately. The schema arrived from `schemas()` as
    // `catalog.schema`, so quoting it whole would name one schema with a dot in
    // it — a different schema, or none. The DuckDB driver has the same two levels
    // and gets away with pasting them unquoted because its catalog names are
    // plain; `TPC-H-small` is not.
    let mut name = String::new();
    for part in what.schema.split('.').filter(|p| !p.is_empty()) {
        name.push_str(&dialect.quote(part));
        name.push('.');
    }
    name.push_str(&dialect.quote(what.relation));
    what.sql_named(dialect, &name)
}

#[async_trait]
impl Driver for FlightSqlSource {
    /// The protocol, because that is all this connection knows. What is behind a
    /// Flight SQL endpoint is not in the connection string — the catalogue entry
    /// says as much — and the one place it could be read from is `GetSqlInfo`,
    /// which this driver does not call.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new("Arrow Flight SQL", ""))
    }
    /// Two levels already flattened, the way DuckDB flattens its own: the
    /// catalog is carried in the `SchemaInfo` name.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(FlightSqlSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(FlightSqlSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(FlightSqlSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(FlightSqlSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(FlightSqlSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always: Flight SQL has a command for the primary key and none for
    /// a unique constraint, so a server that enforces one cannot say so. See
    /// `metadata.rs`.
    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(FlightSqlSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(FlightSqlSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(FlightSqlSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(FlightSqlSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(FlightSqlSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            FlightSqlSource::query(self, statement, batch_rows).await?,
        ))
    }

    fn browse(&self, what: &Browse<'_>) -> String {
        browse_sql(what)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            FlightSqlSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(FlightSqlSource::cancel(self).await?)
    }

    /// Yes, and without a connection held back — which is the one place this
    /// driver answers a trait question differently from every other one here.
    ///
    /// The trait notes that a transaction is really a property of a connection,
    /// so this is a question about the arrangement inside the driver. Flight SQL
    /// makes it a property of a token instead: `ActionBeginTransaction` answers
    /// with a handle and `CommandStatementQuery` carries it, so two statements
    /// are in the same transaction because they name it, not because they went
    /// down the same socket. Nothing here is pooled or pinned and the transaction
    /// still holds.
    ///
    /// Cancel does not reach the statement, and this is the other driver where it
    /// does not. Flight SQL has an action for it that this build does not send:
    /// `cancel` stops this side's reads, the `DoGet` stream is dropped, and the
    /// reset that follows is the only thing the server hears.
    ///
    /// Routines are not reported, and here there is nothing to ask. Flight SQL
    /// defines the catalog commands a client may send — `CommandGetTables`,
    /// `CommandGetDbSchemas`, `CommandGetPrimaryKeys` and the rest — and none of
    /// them is about a function or a procedure. A server behind this protocol may
    /// well have them; the protocol has no way to say so.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: true,
            cancel_stops_the_statement: false,
            switches_database: false,
            schema_is_the_database: false,
            reports_routines: false,
        }
    }

    async fn transaction(&self, step: &TxStep) -> DbResult<()> {
        Ok(FlightSqlSource::transaction(self, step).await?)
    }
}

/// `Rows` is both, because against a Flight ticket a cursor and a result stream
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

    /// The catalog level, which the trait has nowhere to put and this driver
    /// folds into the schema name. Quoting the two-part name as one identifier
    /// would name a schema with a dot in it, which is a different schema or none.
    #[test]
    fn a_catalog_and_a_schema_are_two_names_and_not_one() {
        let keys = ["id".to_string()];
        assert_eq!(
            browse_sql(&browse("TPC-H-small.main", None, None, &keys, None)),
            r#"SELECT * FROM "TPC-H-small".main.orders ORDER BY id"#
        );
    }

    /// A server with no catalog level gets a two-part name, and one with no
    /// schema level at all gets a bare relation rather than a leading dot.
    #[test]
    fn a_server_with_fewer_levels_does_not_get_an_empty_one() {
        assert_eq!(
            browse_sql(&browse("main", None, None, &[], None)),
            "SELECT * FROM main.orders"
        );
        assert_eq!(
            browse_sql(&browse("", None, None, &[], None)),
            "SELECT * FROM orders"
        );
    }

    /// The user's own words reach the statement as typed; the key columns are
    /// this side's and are quoted. A row ceiling is a `LIMIT` at the end, which
    /// is where the standard puts it.
    #[test]
    fn the_users_order_comes_first_and_the_key_makes_it_total() {
        let keys = ["Id".to_string()];
        assert_eq!(
            browse_sql(&browse(
                "main",
                Some("qty > 10"),
                Some("label desc"),
                &keys,
                Some(1000)
            )),
            r#"SELECT * FROM main.orders WHERE qty > 10 ORDER BY label desc, "Id" LIMIT 1000"#
        );
    }
}
