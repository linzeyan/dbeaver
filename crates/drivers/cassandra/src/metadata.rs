//! What Cassandra can say about itself, in the shape the navigator expects.
//!
//! `system_schema` is the catalog and it is an ordinary keyspace: every question
//! here is a `SELECT` against a table anybody can read, restricted on
//! `keyspace_name` and then on `table_name`, which are the partition key and the
//! first clustering column of every one of them. That is why none of these needs
//! `ALLOW FILTERING` and why a relation that is not there is an empty result
//! rather than a failure — the restriction simply matches nothing.
//!
//! **Three of the nine calls answer with nothing and issue no query to find
//! out.** Each is a statement about the database rather than a gap here:
//!
//! - There are no foreign keys. Cassandra has no join to enforce one against,
//!   and denormalising the reference into the row is the modelling advice rather
//!   than a workaround. `foreign_keys` and `referenced_by` are therefore empty,
//!   and a round trip proving it would be a round trip spent proving something
//!   already known. This is MongoDB's situation and it gets MongoDB's answer.
//! - There are no constraints. Not unenforced ones — none: `system_schema` has
//!   no table for them, `CHECK` is not in the 5.0 grammar, and the nearest
//!   things Cassandra has are guardrails, which are cluster settings rather than
//!   objects attached to a table. Unlike MongoDB, whose collection validator
//!   turned out to be a check constraint wearing another name, there is nothing
//!   here to report.
//!
//! `triggers` is the one that is not empty and that nobody will see. CQL has
//! `CREATE TRIGGER`, `system_schema.triggers` records it, and this reads it —
//! but a trigger is a Java class on the server's classpath, so no test in this
//! repository can create one without shipping a JAR into the container. The call
//! is written and exercised against a table that has none, which is the case a
//! driver is most likely to get wrong by failing instead.
//!
//! Every statement is sent with bound values rather than with the names
//! interpolated. That costs a round trip, because an unprepared statement with
//! values has to be prepared first — and it is worth it: a keyspace or table
//! name reaching here came from the user's own tree, and the alternative is this
//! file owning a CQL string-escaping routine that has to be right every time.

use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    SchemaInfo, TriggerInfo, UniqueKeyInfo,
};
use scylla::response::query_result::QueryRowsResult;
use scylla::serialize::row::SerializeRow;
use std::collections::HashMap;

use crate::{CassandraError, CassandraSource};

/// What `system_schema.columns.kind` calls a column that is part of the primary
/// key, and the order the two go in.
///
/// Partition key first and clustering second, because that is the order the key
/// is written in and the order a reader looks for it in. `static` and `regular`
/// share the last rank: both are ordinary columns of the row, and Cassandra
/// gives neither a position of its own.
fn rank(kind: &str) -> u8 {
    match kind {
        "partition_key" => 0,
        "clustering" => 1,
        _ => 2,
    }
}

impl CassandraSource {
    /// Runs one catalog query and hands back the rows for the caller to type.
    ///
    /// No statement position is attached to a failure here. The offset would be
    /// into `SELECT … FROM system_schema.columns …`, which is text the user
    /// never wrote, and a caret placed from it would land wherever their own
    /// statement happened to reach.
    async fn ask(
        &self,
        cql: &str,
        values: impl SerializeRow,
    ) -> Result<QueryRowsResult, CassandraError> {
        self.session()
            .query_unpaged(cql, values)
            .await
            .map_err(|e| CassandraError::from_server(e, None))?
            .into_rows_result()
            .map_err(|e| CassandraError::Request {
                message: e.to_string(),
                position: None,
            })
    }

    /// The keyspaces on this cluster.
    ///
    /// All of them, `system_schema` and friends included. Hiding them was
    /// considered and rejected for the reason MongoDB lists `admin` and
    /// `config`: the catalog this file reads lives in one of them, so a
    /// navigator that hid it would be hiding the tables that answer every other
    /// question on this page — and `system_views.settings` and
    /// `system.size_estimates` are things people genuinely open a client to
    /// look at.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, CassandraError> {
        let result = self
            .ask("SELECT keyspace_name FROM system_schema.keyspaces", &[])
            .await?;
        let mut out = Vec::new();
        for row in result.rows::<(String,)>().map_err(text)? {
            let (name,) = row.map_err(text)?;
            // Every keyspace Cassandra ships is named `system` or begins
            // `system_`: the schema catalog this file reads, the auth tables,
            // the traces, the virtual tables. One prefix covers them, and a
            // keyspace an application called `system_of_record` would be caught
            // by it — accepted, because the alternative is a list of seven names
            // that a version bump adds an eighth to.
            let is_system = name == "system" || name.starts_with("system_");
            out.push(SchemaInfo { name, is_system });
        }
        Ok(out)
    }

    /// The tables and materialized views in one keyspace.
    ///
    /// Two queries because Cassandra keeps them in two tables, and a
    /// materialized view really is a relation: it has its own columns, its own
    /// primary key, its own SSTables, and it is what a `SELECT` names. Listing
    /// it under `Table` would offer the wrong actions for something that cannot
    /// be written to directly.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, CassandraError> {
        let mut out = Vec::new();
        for (cql, kind) in [
            (
                "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
                RelationKind::Table,
            ),
            (
                "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
                RelationKind::MaterializedView,
            ),
        ] {
            let result = self.ask(cql, (schema,)).await?;
            for row in result.rows::<(String,)>().map_err(text)? {
                let (name,) = row.map_err(text)?;
                out.push(RelationInfo {
                    schema: schema.to_string(),
                    name,
                    kind,
                    // Deliberately not filled. `system.size_estimates` is the
                    // only count Cassandra offers and it is per node and per
                    // token range — on a cluster it describes the ranges this
                    // one node happens to own, so summing it would report a
                    // fraction of the table as the whole of it. `None` means
                    // "nothing has measured this", which is exactly the case.
                    estimated_rows: None,
                });
            }
        }
        Ok(out)
    }

    /// The columns of a table or a materialized view.
    ///
    /// Ordered partition key, then clustering, then the rest — which is not the
    /// order the catalog returns them in. `system_schema.columns` is clustered
    /// on `column_name`, so it answers alphabetically, and its `position` counts
    /// from zero *within each kind*: the first partition key column and the
    /// first clustering column are both position 0, and every ordinary column is
    /// -1. Handing that to a structure pane would put `bucket` after `label` and
    /// number two different columns the same.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, CassandraError> {
        let result = self
            .ask(
                "SELECT column_name, kind, position, type FROM system_schema.columns \
                 WHERE keyspace_name = ? AND table_name = ?",
                (schema, relation),
            )
            .await?;

        let mut found: Vec<(String, String, i32, String)> = Vec::new();
        for row in result
            .rows::<(String, String, i32, String)>()
            .map_err(text)?
        {
            found.push(row.map_err(text)?);
        }
        // Stable, so the ordinary columns keep the alphabetical order the
        // catalog returned them in rather than an arbitrary one: they all share
        // a rank and a position, and only the sort's stability decides between
        // them.
        found.sort_by_key(|(_, kind, position, _)| (rank(kind), *position));

        Ok(found
            .into_iter()
            .enumerate()
            .map(|(at, (name, kind, _, declared))| {
                let is_primary_key = rank(&kind) < 2;
                ColumnInfo {
                    name,
                    data_type: declared,
                    // A primary key column cannot hold a null in CQL and every
                    // other column can, so this is read off the kind rather than
                    // from a catalog column — there is none to read.
                    nullable: !is_primary_key,
                    position: at as i32 + 1,
                    is_primary_key,
                    // CQL has no column defaults. A missing value on insert
                    // leaves no cell at all rather than writing something in its
                    // place, which is a stronger statement than "the default is
                    // null".
                    default_value: None,
                    computed: None,
                }
            })
            .collect())
    }

    /// The `WHERE` clause a materialized view is defined by, or `None` for a
    /// table.
    ///
    /// The clause and not a whole `CREATE MATERIALIZED VIEW`, because the clause
    /// is what the catalog holds and the rest would have to be reassembled from
    /// four more tables — the selected columns, the primary key, the clustering
    /// order — each of which is a chance to render something that does not quite
    /// recreate the view. It is also the part that carries the meaning: an MV
    /// selects every row of its base table whose key columns are non-null, and
    /// the clause is where any further restriction lives.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, CassandraError> {
        let result = self
            .ask(
                "SELECT where_clause FROM system_schema.views \
                 WHERE keyspace_name = ? AND view_name = ?",
                (schema, relation),
            )
            .await?;
        // At most one row: the restriction is the whole primary key of
        // `system_schema.views`, so a name that is not a view is no rows rather
        // than an error.
        let Some(row) = result.rows::<(String,)>().map_err(text)?.next() else {
            return Ok(None);
        };
        let (clause,) = row.map_err(text)?;
        Ok((!clause.is_empty()).then_some(clause))
    }

    /// The secondary indexes on a table.
    ///
    /// None of them is unique and none of them is the primary key, and both
    /// facts are about Cassandra rather than about this table. A secondary index
    /// is a hidden table keyed on the indexed value holding the primary keys
    /// that carry it, so several rows sharing a value is the case it is built
    /// for; and the primary key is how the data is partitioned rather than an
    /// index over it, so there is no object here to call primary. Reporting
    /// otherwise would tell a reader the planner can do a lookup it cannot.
    /// Empty, always, and without asking.
    ///
    /// CQL has one uniqueness constraint and it is the primary key, which
    /// `columns` reports already. A secondary index does not make its column
    /// unique — see `indexes`, where several rows sharing a value is the case it
    /// exists for — and there is no `UNIQUE` in the `CREATE TABLE` grammar to
    /// declare one with.
    pub async fn unique_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, CassandraError> {
        Ok(Vec::new())
    }

    pub async fn indexes(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<IndexInfo>, CassandraError> {
        let result = self
            .ask(
                "SELECT index_name, kind, options FROM system_schema.indexes \
                 WHERE keyspace_name = ? AND table_name = ?",
                (schema, relation),
            )
            .await?;

        let mut out = Vec::new();
        for row in result
            .rows::<(String, String, HashMap<String, String>)>()
            .map_err(text)?
        {
            let (name, kind, options) = row.map_err(text)?;
            out.push(IndexInfo {
                name,
                // `target` is an expression and not always a bare name:
                // `keys(m)`, `values(m)` and `entries(m)` are how a map is
                // indexed three different ways, and an index on `keys(m)` is not
                // an index on `m`. Printing it as one would claim a lookup the
                // planner cannot do — which is the reason `IndexInfo::columns`
                // holds expressions.
                columns: options
                    .get("target")
                    .filter(|target| !target.is_empty())
                    .map_or_else(Vec::new, |target| vec![target.clone()]),
                is_unique: false,
                is_primary: false,
                // The implementing class for a custom index, and the catalog's
                // own word otherwise. `CUSTOM` alone says nothing — SAI and SASI
                // are both custom and answer completely different queries — so
                // the class name is the informative answer where there is one.
                method: match (kind.as_str(), options.get("class_name")) {
                    ("CUSTOM", Some(class)) if !class.is_empty() => class.clone(),
                    _ => kind.to_ascii_lowercase(),
                },
                // Cassandra has no partial indexes: an index covers every row of
                // its base table.
                predicate: None,
            });
        }
        Ok(out)
    }

    /// Empty, always, and without asking. Cassandra declares no foreign keys —
    /// there is no join to enforce one against, and a reference between tables
    /// lives in the application's model and nowhere the server can see it.
    pub async fn foreign_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, CassandraError> {
        Ok(Vec::new())
    }

    /// Empty for the same reason as `foreign_keys`: nothing is declared at the
    /// other end either, so there is nothing to look up from it.
    pub async fn referenced_by(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, CassandraError> {
        Ok(Vec::new())
    }

    /// Empty, always. Cassandra 5.0 has no constraints of any kind — see the
    /// module comment for why this is not the MongoDB case, where a collection's
    /// validator turned out to be a check constraint under another name.
    pub async fn constraints(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<ConstraintInfo>, CassandraError> {
        Ok(Vec::new())
    }

    /// The triggers on a table.
    ///
    /// Cassandra records a trigger's name and the Java class that implements it,
    /// and nothing else: there is no timing, no event list and no level, because
    /// a trigger is not declared against any of them — it is handed every
    /// mutation that reaches the coordinator for that table and decides for
    /// itself. So four of `TriggerInfo`'s fields are `None` and one is an empty
    /// list, which is the shape that field grew for exactly this.
    pub async fn triggers(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<TriggerInfo>, CassandraError> {
        let result = self
            .ask(
                "SELECT trigger_name, options FROM system_schema.triggers \
                 WHERE keyspace_name = ? AND table_name = ?",
                (schema, relation),
            )
            .await?;

        let mut out = Vec::new();
        for row in result
            .rows::<(String, HashMap<String, String>)>()
            .map_err(text)?
        {
            let (name, options) = row.map_err(text)?;
            let class = options.get("class").cloned();
            out.push(TriggerInfo {
                // The statement, reassembled. Doing this is normally a mistake —
                // `ConstraintInfo` says so — but a CQL trigger has no
                // expression, no body and no options beyond the class, so the
                // three catalog values *are* the statement and there is nothing
                // in it to render wrongly.
                definition: class.as_ref().map(|class| {
                    format!("CREATE TRIGGER {name} ON {schema}.{relation} USING '{class}'")
                }),
                function: class,
                name,
                timing: None,
                events: Vec::new(),
                level: None,
                // Cassandra has no way to disable a trigger; dropping it is the
                // only off switch, so one that is listed is one that fires.
                enabled: true,
            });
        }
        Ok(out)
    }
}

/// A deserialization failure as a plain message.
///
/// These are type mismatches against `system_schema`, which mean the catalog is
/// not the shape this file was written against — a Cassandra release having
/// changed it, or a server that is not Cassandra. Nothing about them is a
/// position in a statement the user wrote.
fn text<E: std::fmt::Display>(error: E) -> CassandraError {
    CassandraError::Request {
        message: error.to_string(),
        position: None,
    }
}
