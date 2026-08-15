//! What Trino can say about itself, in the shape the navigator expects.
//!
//! `information_schema` is the catalog, and there is one per catalog — a
//! connector's own, assembled by the coordinator from whatever the system behind
//! it can be asked. So every question below names the catalog it is about
//! (`"tpch".information_schema.columns`) rather than a single global place, and
//! the restriction is always on `table_schema` and then `table_name`, which is
//! what makes a relation that is not there an empty result instead of a failure.
//!
//! **Five of the nine calls answer with nothing and send no statement to find
//! out.** For once none of that is a judgement call: Trino's `information_schema`
//! has exactly eight tables — `applicable_roles`, `columns`, `enabled_roles`,
//! `roles`, `schemata`, `table_privileges`, `tables`, `views` — and asking for
//! any of the six a catalog would need is `TABLE_NOT_FOUND`:
//!
//! - There are no indexes. Not hidden ones, not connector-specific ones:
//!   `information_schema.statistics` does not exist, and `CREATE INDEX` is a
//!   *syntax error* whose message lists what `CREATE` accepts — BRANCH, CATALOG,
//!   FUNCTION, MATERIALIZED, OR, ROLE, SCHEMA, TABLE, VIEW. A connector's own
//!   indexes are real and are invisible here by design; Trino's job is to plan
//!   around them, not to describe them.
//! - There are no keys of any kind. `table_constraints`,
//!   `referential_constraints` and `key_column_usage` are all absent, and
//!   `CREATE TABLE t (n integer PRIMARY KEY)` is a syntax error at the words
//!   `PRIMARY KEY`. So `foreign_keys`, `referenced_by` and `constraints` are
//!   empty, and `ColumnInfo::is_primary_key` is false for every column of every
//!   table this driver will ever report.
//! - There are no triggers. `CREATE TRIGGER` is the same syntax error as
//!   `CREATE INDEX`, and `information_schema.triggers` does not exist.
//!
//! The one constraint-shaped fact Trino does have is `is_nullable`, and it is
//! already on the column where it belongs rather than in `constraints()` as a
//! synthesised `NOT NULL` object nobody could drop.
//!
//! Every statement here interpolates its schema and table names as string
//! literals rather than binding them. `crate::literal` argues that trade; the
//! short version is that Trino's parameters travel in a request header and cost
//! a rewrite of every statement below, to replace one function that doubles a
//! quote.

use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    SchemaInfo, TriggerInfo, UniqueKeyInfo,
};
use serde_json::Value;

use crate::{TrinoError, TrinoSource, literal, parts, quote};

/// `information_schema.tables.table_type`, as one of the kinds the navigator
/// knows about.
///
/// Two values and no more: a sweep of every catalog on a stock coordinator
/// returns `BASE TABLE` and `VIEW`. `Unknown` for anything else is not
/// defensive padding — Trino has materialized views, no connector in that sweep
/// can create one, and nothing in this repository can establish which of the two
/// strings one reports itself as. Guessing `Table` would offer a structure pane
/// the actions of a table for something that is refreshed rather than written;
/// `Unknown` says what is true, which is that this driver has not met it.
fn relation_kind(table_type: &str) -> RelationKind {
    match table_type {
        "BASE TABLE" => RelationKind::Table,
        "VIEW" => RelationKind::View,
        _ => RelationKind::Unknown,
    }
}

/// One column of a catalog row, as text.
///
/// A null becomes an empty string rather than an error. Every column read below
/// is a name or a type that the coordinator fills in for every row, so a null
/// here would mean the catalog is not the shape this file was written against —
/// and an empty name is visible in the navigator, where a failed refresh is not.
fn text(row: &[Value], at: usize) -> String {
    match row.get(at) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn number(row: &[Value], at: usize) -> i64 {
    row.get(at).and_then(Value::as_i64).unwrap_or_default()
}

impl TrinoSource {
    /// Every schema on this coordinator, as `catalog.schema`.
    ///
    /// One statement rather than a `SHOW CATALOGS` followed by one
    /// `information_schema.schemata` per catalog. `system.jdbc.schemas` is the
    /// view Trino keeps for exactly this — the JDBC driver's `getSchemas()` —
    /// and it answers for every catalog at once, which on a coordinator with
    /// forty of them is the difference between one round trip and forty.
    ///
    /// Nothing is hidden, `information_schema` included. Hiding it was
    /// considered and rejected for the reason the Cassandra driver gives about
    /// `system_schema`: the catalog that answers every other question on this
    /// page lives there, so a navigator that hid it would be hiding the tables
    /// it is built on — and `system.runtime.queries`, which is where somebody
    /// goes to find out what is holding the cluster up, is in the same position.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, TrinoError> {
        let rows = self
            .ask(
                "SELECT table_catalog, table_schem FROM system.jdbc.schemas \
                 ORDER BY table_catalog, table_schem",
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| SchemaInfo {
                name: format!("{}.{}", text(row, 0), text(row, 1)),
            })
            .collect())
    }

    /// The tables and views in one `catalog.schema`.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, TrinoError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let rows = self
            .ask(&format!(
                "SELECT table_name, table_type FROM {}.information_schema.tables \
                 WHERE table_schema = {} ORDER BY table_name",
                quote(catalog),
                literal(inner)
            ))
            .await?;
        Ok(rows
            .iter()
            .map(|row| RelationInfo {
                schema: schema.to_string(),
                name: text(row, 0),
                kind: relation_kind(&text(row, 1)),
                // Deliberately not filled. Trino's own estimate comes from `SHOW
                // STATS FOR <table>`, which is one statement per table and asks
                // the connector to produce a summary — on a Hive table with no
                // statistics collected that is a scan. A navigator expanding a
                // schema of two hundred tables would issue two hundred of them.
                // `None` means "nothing has measured this", which is exactly the
                // case until somebody opens the table.
                estimated_rows: None,
            })
            .collect())
    }

    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, TrinoError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let rows = self
            .ask(&format!(
                "SELECT column_name, data_type, ordinal_position, is_nullable, column_default \
                 FROM {}.information_schema.columns \
                 WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                quote(catalog),
                literal(inner),
                literal(relation)
            ))
            .await?;

        Ok(rows
            .iter()
            .map(|row| ColumnInfo {
                name: text(row, 0),
                // Trino's own spelling: `varchar(15)`, `decimal(18, 2)`,
                // `row(n integer, w varchar)`. It is the declared type the
                // contract asks for, and it is not what `arrow_map` reads — that
                // one reads the `typeSignature` off the result, because parsing
                // this string back means writing a parser for nested types.
                data_type: text(row, 1),
                // Already one-based in this catalog, so nothing to convert.
                position: number(row, 2) as i32,
                nullable: text(row, 3) == "YES",
                // False for every column of every table. Trino has no primary
                // keys to report — see the module comment, where `PRIMARY KEY` in
                // a `CREATE TABLE` is a syntax error rather than an unenforced
                // declaration.
                is_primary_key: false,
                default_value: match text(row, 4) {
                    empty if empty.is_empty() => None,
                    default => Some(default),
                },
                computed: None,
            })
            .collect())
    }

    /// The statement a view is defined by; `None` for anything else.
    ///
    /// The body without the `CREATE VIEW` around it, which is what
    /// `information_schema.views` holds and what the trait asks for. `SHOW
    /// CREATE VIEW` would give the whole statement including a `SECURITY
    /// DEFINER` clause the user never wrote, and it needs the name pasted into a
    /// statement where this one takes it as a value.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, TrinoError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(None);
        };
        let rows = self
            .ask(&format!(
                "SELECT view_definition FROM {}.information_schema.views \
                 WHERE table_schema = {} AND table_name = {}",
                quote(catalog),
                literal(inner),
                literal(relation)
            ))
            .await?;
        Ok(rows
            .first()
            .map(|row| text(row, 0))
            .filter(|body| !body.is_empty()))
    }

    /// Empty, always, and without asking. Trino enforces no constraint of any
    /// kind: a connector's tables are somebody else's storage, and `UNIQUE` is
    /// not in the `CREATE TABLE` grammar any more than `PRIMARY KEY` is. There
    /// is therefore nothing here that names a row, which is why the Content tab
    /// over Trino is read-only whatever the underlying system enforces.
    pub async fn unique_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, TrinoError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Trino has no indexes: `CREATE INDEX`
    /// is a syntax error and `information_schema.statistics` does not exist.
    pub async fn indexes(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<IndexInfo>, TrinoError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Trino declares no foreign keys —
    /// `PRIMARY KEY` is not in the `CREATE TABLE` grammar, so there is nothing
    /// for one to reference.
    pub async fn foreign_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, TrinoError> {
        Ok(Vec::new())
    }

    /// Empty for the same reason as `foreign_keys`: nothing is declared at the
    /// other end either, so there is nothing to look up from it.
    pub async fn referenced_by(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, TrinoError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Trino has no constraint catalog and no
    /// constraint syntax; the one constraint-shaped fact it keeps is
    /// `is_nullable`, and `columns` already carries it.
    pub async fn constraints(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<ConstraintInfo>, TrinoError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Trino has no triggers: `CREATE
    /// TRIGGER` is a syntax error.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, TrinoError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view is not a table, which is what the structure pane hangs a section
    /// on — and a third kind is said to be unrecognised rather than guessed at.
    #[test]
    fn only_the_two_kinds_trino_actually_reports_are_claimed() {
        assert_eq!(relation_kind("BASE TABLE"), RelationKind::Table);
        assert_eq!(relation_kind("VIEW"), RelationKind::View);
        assert_eq!(relation_kind("MATERIALIZED VIEW"), RelationKind::Unknown);
    }

    /// A null in a catalog column is a name nobody can click rather than a
    /// refresh that fails.
    #[test]
    fn a_missing_catalog_value_reads_as_empty_rather_than_failing() {
        let row = vec![Value::String("orders".into()), Value::Null];
        assert_eq!(text(&row, 0), "orders");
        assert_eq!(text(&row, 1), "");
        assert_eq!(text(&row, 7), "");
        assert_eq!(number(&row, 7), 0);
    }
}
