//! `CassandraSource` seen through the `Driver` trait.
//!
//! As thin as the others except in one place. `browse` is the only method in any
//! driver in this workspace that declines `Browse::sql` on grounds other than
//! not speaking SQL — CQL is close enough to SQL that the shared builder would
//! produce a statement Cassandra parses happily and then refuses. See there.

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, Cursor as CursorApi,
    CursorCancel as CursorCancelApi, DatabaseInfo, DbError, DbResult, Driver, IndexInfo,
    RelationInfo, RelationshipInfo, ResultStream, SchemaInfo, ServerInfo, TriggerInfo, TxStep,
    UniqueKeyInfo, scalar_text,
};

use crate::{CassandraError, CassandraSource, Rows, RowsCancel};

/// The three questions a front end asks of a failure, answered before the rest
/// of the error becomes a string.
impl From<CassandraError> for DbError {
    fn from(e: CassandraError) -> Self {
        let position = e.statement_position();
        let cancelled = e.is_cancelled();
        DbError::new(e.to_string())
            .at_position(position)
            .as_cancelled(cancelled)
    }
}

/// A name as CQL spells one.
///
/// Always quoted, never conditionally. An unquoted CQL identifier folds to lower
/// case, so `SELECT * FROM Orders` reads the table called `orders` — and if the
/// user made one called `Orders` with quotes, it reads a different table, or
/// none, without saying so. The navigator hands over the name the catalog
/// returned, which is the real one, and quoting is what keeps it that way.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The statement a browse reads a relation with.
///
/// A free function rather than only the method body, so the tests below can
/// reach it: `browse` promises no I/O and there is nothing in here that needs a
/// session, but the trait method needs a `CassandraSource` and a `CassandraSource` needs a
/// server. A test that rebuilt the statement itself would be checking a copy.
fn browse_cql(what: &Browse<'_>) -> String {
    let mut cql = String::from("SELECT * FROM ");
    // A keyspace is always there in practice, since the navigator has one to
    // click on; empty is what a connection with no keyspace and a hand-written
    // browse would give, and `."orders"` is not a table anywhere.
    if !what.schema.is_empty() {
        cql.push_str(&quote(what.schema));
        cql.push('.');
    }
    cql.push_str(&quote(what.relation));

    if let Some(filter) = what.filter.map(str::trim).filter(|f| !f.is_empty()) {
        cql.push_str(" WHERE ");
        cql.push_str(filter);
    }
    if let Some(order) = what.order.map(str::trim).filter(|o| !o.is_empty()) {
        cql.push_str(" ORDER BY ");
        cql.push_str(order);
    }
    if let Some(rows) = what.limit {
        cql.push_str(&format!(" LIMIT {rows}"));
    }
    cql
}

#[async_trait]
impl Driver for CassandraSource {
    /// The release version out of `system.local`, which is the one row every node
    /// keeps about itself. CQL has no `version()` to call.
    async fn server_info(&self) -> DbResult<ServerInfo> {
        Ok(ServerInfo::new(
            "Cassandra",
            scalar_text(self, "SELECT release_version FROM system.local").await?,
        ))
    }
    /// A keyspace is Cassandra's single level and `schemas()` lists keyspaces.
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        Ok(CassandraSource::schemas(self).await?)
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        Ok(CassandraSource::relations(self, schema).await?)
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        Ok(CassandraSource::columns(self, schema, relation).await?)
    }

    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>> {
        Ok(CassandraSource::definition(self, schema, relation).await?)
    }

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(CassandraSource::indexes(self, schema, relation).await?)
    }

    /// Empty, always: CQL's only uniqueness is the primary key, and a secondary
    /// index does not make its column unique. See `metadata.rs`.
    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        Ok(CassandraSource::unique_keys(self, schema, relation).await?)
    }

    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(CassandraSource::foreign_keys(self, schema, relation).await?)
    }

    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(CassandraSource::referenced_by(self, schema, relation).await?)
    }

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(CassandraSource::constraints(self, schema, relation).await?)
    }

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(CassandraSource::triggers(self, schema, relation).await?)
    }

    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>> {
        Ok(Box::new(
            CassandraSource::query(self, statement, batch_rows).await?,
        ))
    }

    /// `SELECT * FROM "ks"."tbl"`, written here rather than by `Browse::sql`.
    ///
    /// The reason is `what.keys`, which this ignores. Every other driver appends
    /// the key columns to the `ORDER BY` so that a browse looks the same twice —
    /// without a total order the rows come back in whatever order the plan
    /// produced, which is stable within one read and arbitrary between two. In
    /// CQL that fix is not available: `ORDER BY` is legal only on clustering
    /// columns and only inside one partition, so a browse of a whole table with
    /// the key appended is refused outright with *"ORDER BY is only supported
    /// when the partition key is restricted by an EQ or an IN"*. A statement
    /// that fails is worse than one whose row order moves, so the order is left
    /// as Cassandra returns it: by token of the partition key, which is a hash
    /// and therefore looks random and is not.
    ///
    /// Everything else is ordinary. `what.order` is the user's own words and
    /// goes in as typed — a browse restricted to one partition *can* be ordered,
    /// and this is where they say so. `what.filter` likewise: a filter on a
    /// column that is not part of the key will be refused by the server telling
    /// them to add `ALLOW FILTERING`, which is Cassandra's own warning that the
    /// query reads the whole table, and it is better heard from Cassandra than
    /// quietly satisfied here. `LIMIT` is real CQL and means what it says.
    fn browse(&self, what: &Browse<'_>) -> String {
        browse_cql(what)
    }

    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn CursorApi>> {
        Ok(Box::new(
            CassandraSource::cursor(self, statement, batch_rows).await?,
        ))
    }

    async fn cancel(&self) -> DbResult<()> {
        Ok(CassandraSource::cancel(self).await?)
    }

    /// No, and not for want of a connection to hold one on.
    ///
    /// Cassandra has no interactive transaction. The two things people reach for
    /// instead are neither of them one: a lightweight transaction is a single
    /// statement's compare-and-set, decided by Paxos among the replicas of one
    /// partition and finished by the time the statement answers; and a `BATCH`
    /// is atomic across partitions but is written whole and sent whole, so there
    /// is no moment between opening it and committing it in which a client could
    /// run a `SELECT` and look. `BEGIN` has nothing to name.
    ///
    /// Cancel does not reach the statement, and this is one of the two drivers
    /// here where it does not. CQL has no cancel: `cancel` stops this side's
    /// reads, the fetch in flight resolves as cancelled, and the coordinator goes
    /// on assembling the page it was asked for and drops it on the floor.
    ///
    /// Routines are not reported, and that is a gap: a keyspace can hold
    /// user-defined functions and aggregates, and `system_schema.functions` and
    /// `system_schema.aggregates` are where they are. They would also arrive in
    /// two lists rather than one, which is the part that needs deciding before
    /// this can be true.
    ///
    /// Sequences are not reported, and there is no such object. A counter column
    /// is the nearest thing and belongs to a table; nothing a keyspace holds
    /// hands out increasing numbers.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: false,
            switches_database: false,
            schema_is_the_database: false,
            reports_routines: false,
            reports_sequences: false,
        }
    }

    /// Refused rather than skipped, so that nobody is told a transaction is open
    /// when nothing is.
    async fn transaction(&self, _step: &TxStep) -> DbResult<()> {
        Err(DbError::new(
            "Cassandra has no transaction to control: a lightweight transaction is one \
             statement's compare-and-set, and a BATCH is atomic but is sent whole rather \
             than opened and committed later",
        ))
    }
}

/// `Rows` is both, because against Cassandra a cursor and a result stream are
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
        filter: Option<&'a str>,
        order: Option<&'a str>,
        keys: &'a [String],
        limit: Option<u32>,
    ) -> Browse<'a> {
        Browse {
            schema: "bench",
            relation: "orders",
            filter,
            order,
            keys,
            limit,
        }
    }

    use browse_cql as statement;

    /// The one this driver exists to get right. Appending the key is what every
    /// other driver does and what makes Cassandra refuse the statement.
    #[test]
    fn the_key_is_not_appended_to_an_order_by() {
        let keys = ["id".to_string()];
        assert_eq!(
            statement(&browse(None, None, &keys, None)),
            r#"SELECT * FROM "bench"."orders""#
        );
    }

    /// The user's own order is still theirs — it is legal CQL inside one
    /// partition, and this is the only way to write it.
    #[test]
    fn the_users_own_order_reaches_the_statement_as_typed() {
        let keys = ["id".to_string()];
        assert_eq!(
            statement(&browse(Some("bucket = 0"), Some("id desc"), &keys, None)),
            r#"SELECT * FROM "bench"."orders" WHERE bucket = 0 ORDER BY id desc"#
        );
    }

    /// `LIMIT` is real CQL and goes where CQL puts it: last, after any ordering.
    #[test]
    fn a_row_ceiling_is_a_limit_at_the_end() {
        assert_eq!(
            statement(&browse(None, None, &[], Some(1000))),
            r#"SELECT * FROM "bench"."orders" LIMIT 1000"#
        );
    }

    /// Unquoted CQL folds to lower case, so a name that was created with quotes
    /// has to keep them — and a quote inside one is doubled, as in SQL.
    #[test]
    fn a_name_that_is_not_lower_case_survives_being_written_down() {
        let what = Browse {
            schema: "Bench",
            relation: "we\"ird",
            filter: None,
            order: None,
            keys: &[],
            limit: None,
        };
        assert_eq!(statement(&what), r#"SELECT * FROM "Bench"."we""ird""#);
    }
}
