//! What ClickHouse can say about itself, in the shape the navigator expects.
//!
//! The `system` database is the catalog. `INFORMATION_SCHEMA` exists beside it
//! and is a compatibility shim — `REFERENTIAL_CONSTRAINTS` and `STATISTICS` are
//! documented as *always* returning no rows, so that third-party tools do not
//! error — so nothing here reads it, and both are hidden from the navigator
//! along with `system` itself.
//!
//! **Four of the nine calls return nothing and issue no query to find out.**
//! That is a statement about the database and not a gap in this file:
//!
//! - There are no foreign keys. Not unenforced ones, not declared-and-ignored
//!   ones. Upstream says so twice — `supports-references` is `false` in
//!   `plugin.xml` and `supportsReferentialIntegrity()` returns `false` — and
//!   `INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS` is the empty view above. So
//!   `foreign_keys` and `referenced_by` are empty, and a round trip that proved
//!   it would be a round trip spent proving something already known.
//! - There are no triggers. A materialized view fires on insert into its source
//!   table and is the nearest thing, but it is a relation — it is already in
//!   `relations()` as one — and listing it again here would report one object as
//!   two.
//! - There are constraints, and there is nowhere to read them from. `CONSTRAINT
//!   name CHECK expr` is real and `ALTER TABLE … ADD CONSTRAINT` adds it, but
//!   there is no `system.constraints`, and `INFORMATION_SCHEMA` has neither
//!   `TABLE_CONSTRAINTS` nor `CHECK_CONSTRAINTS`. The only place one appears is
//!   inside the text of `create_table_query`, and pulling it out of there means
//!   parsing SQL — which is `crates/sql`'s job in phase 3 and not something to
//!   half-do here. This is SQLite's situation exactly and it gets SQLite's
//!   answer.
//!
//! What is *not* empty is `indexes`, and that is a gain over upstream rather
//! than a port of it: `ClickhouseDataSourceInfo.supportsIndexes()` returns
//! `false` with the comment *"Clickhouse driver return us empty list as
//! indexInfo … So far we turn off indexes"*. Skip indexes are real, queryable
//! and worth showing.

use clickhouse::{Client, Row};
use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    SchemaInfo, TriggerInfo,
};
use serde::Deserialize;

use crate::{ChError, ChSource, arrow_map};

/// The facts about a ClickHouse table that `RelationInfo` has no field for.
///
/// Separate rather than squeezed into the shared struct, and reported through a
/// call of its own — see `ChSource::storage`. Every field here is something a
/// structure pane would otherwise have to invent or omit.
#[derive(Debug, Clone, PartialEq)]
pub struct Storage {
    /// MergeTree, ReplacingMergeTree, Distributed, View, Dictionary, Log, …
    pub engine: String,
    /// The `ORDER BY` expression: what the data is physically sorted by, and the
    /// only thing resembling an index that ClickHouse will use for a range scan.
    ///
    /// Deliberately not reported through `indexes()`. It is a property of how
    /// the table is stored rather than a droppable named object, and it has no
    /// name to put in `IndexInfo::name` — synthesising one called `PRIMARY`
    /// would put a fabricated object in a list of catalog objects and invite
    /// somebody to try to drop it.
    pub sorting_key: Option<String>,
    /// Usually a prefix of the sorting key, and **not** a uniqueness
    /// constraint: ClickHouse's primary key is the sparse index over the sorted
    /// data, duplicates are ordinary, and `ReplacingMergeTree` exists precisely
    /// because they are.
    pub primary_key: Option<String>,
    pub partition_key: Option<String>,
    pub comment: Option<String>,
}

#[derive(Row, Deserialize)]
struct NameRow {
    name: String,
}

#[derive(Row, Deserialize)]
struct RelationRow {
    name: String,
    engine: String,
    total_rows: Option<u64>,
}

#[derive(Row, Deserialize)]
struct StorageRow {
    engine: String,
    sorting_key: String,
    primary_key: String,
    partition_key: String,
    comment: String,
}

#[derive(Row, Deserialize)]
struct DefinitionRow {
    engine: String,
    as_select: String,
    create_table_query: String,
}

#[derive(Row, Deserialize)]
struct ColumnRow {
    name: String,
    #[serde(rename = "type")]
    declared: String,
    position: u64,
    default_kind: String,
    default_expression: String,
    is_in_primary_key: u8,
}

#[derive(Row, Deserialize)]
struct IndexRow {
    name: String,
    type_full: String,
    expr: String,
    granularity: u64,
}

/// `system.tables.engine`, as one of the kinds the navigator knows about.
///
/// The engine decides this; there is no `table_type` column to read. Upstream
/// reaches almost the same conclusion the crude way — it selects `engine as
/// TABLE_TYPE` and then asks whether the string contains "VIEW", which lumps a
/// materialized view in with a plain one. They behave differently enough (a
/// materialized view holds data; a view does not) that conflating them is a lie
/// the structure pane would repeat on every refresh.
fn relation_kind(engine: &str) -> RelationKind {
    match engine {
        "View" | "LiveView" | "WindowView" => RelationKind::View,
        "MaterializedView" => RelationKind::MaterializedView,
        // A dictionary is queryable but is neither a table nor a view: its rows
        // come from a source outside this server and are refreshed on a
        // schedule. `Virtual` is the closest honest kind the shared enum has —
        // "rows come from an extension rather than from storage" — and it is
        // nearer than reporting it as an ordinary table somebody could `ALTER`.
        "Dictionary" => RelationKind::Virtual,
        _ => RelationKind::Table,
    }
}

fn some_unless_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

impl ChSource {
    /// The databases on this server, minus the two that are catalog rather than
    /// data.
    ///
    /// Upstream reads `SHOW DATABASES` instead, with a comment naming a JDBC
    /// catalog bug as the reason. We are not on JDBC, and the system table is
    /// the better source anyway: it takes a `WHERE` and `SHOW` does not.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, ChError> {
        let rows = self
            .client
            .query(
                "SELECT name FROM system.databases \
                 WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') \
                 ORDER BY name",
            )
            .fetch_all::<NameRow>()
            .await
            .map_err(|e| ChError::from_server(e, None))?;
        Ok(rows
            .into_iter()
            .map(|r| SchemaInfo { name: r.name })
            .collect())
    }

    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, ChError> {
        let rows = self
            .client
            .query(
                "SELECT name, engine, total_rows FROM system.tables \
                 WHERE database = ? ORDER BY name",
            )
            .bind(schema)
            .fetch_all::<RelationRow>()
            .await
            .map_err(|e| ChError::from_server(e, None))?;

        Ok(rows
            .into_iter()
            .map(|r| RelationInfo {
                schema: schema.to_string(),
                kind: relation_kind(&r.engine),
                name: r.name,
                // `total_rows` is the sum over active parts for the MergeTree
                // family and NULL for every view and every engine that keeps no
                // count. That NULL is carried through as `None` rather than
                // clamped to 0 — declining to answer is not the same as
                // answering zero, and only one of them is true.
                estimated_rows: r.total_rows.map(|n| n as i64),
            })
            .collect())
    }

    pub async fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>, ChError> {
        let rows = self
            .client
            .query(
                "SELECT name, type, position, default_kind, default_expression, \
                        is_in_primary_key \
                 FROM system.columns WHERE database = ? AND table = ? ORDER BY position",
            )
            .bind(schema)
            .bind(relation)
            .fetch_all::<ColumnRow>()
            .await
            .map_err(|e| ChError::from_server(e, None))?;

        Ok(rows
            .into_iter()
            .map(|r| ColumnInfo {
                nullable: arrow_map::is_nullable(&r.declared),
                // `system.columns.type` verbatim: `Nullable(Decimal(18, 4))`,
                // `Array(LowCardinality(String))`, `Enum8('a' = 1, 'b' = 2)`.
                // It is the declared type the contract asks for, and it is also
                // what `arrow_map` reads, so it is load-bearing twice.
                data_type: r.declared,
                // Already one-based in this catalog, so nothing to convert.
                position: r.position as i32,
                // What the user wrote after `PRIMARY KEY`/`ORDER BY`, and what
                // the planner uses — but **not** a uniqueness claim. A structure
                // pane that renders a key icon meaning "unique" beside this is
                // saying something ClickHouse never promised.
                is_primary_key: r.is_in_primary_key != 0,
                default_value: default_of(&r.default_kind, r.default_expression),
                // `default_of` already writes the kind into the text —
                // `MATERIALIZED a+b`, `ALIAS a+b` — so a reader of that field
                // can see what it is looking at, and this crate writes no
                // column declarations: ClickHouse DDL is the statement the
                // server kept.
                computed: None,
                name: r.name,
            })
            .collect())
    }

    /// The statement a view is defined by; `None` for anything else.
    ///
    /// Read from `system.tables` rather than from `SHOW CREATE TABLE`, which
    /// upstream uses and then reformats by hand (`normalizeDDL` splits on the
    /// first `(` and on `") ENGINE"`, so a table whose first column is
    /// `Decimal(18, 4)` comes out mangled). This one takes a bound parameter
    /// where `SHOW CREATE` needs the identifier pasted into the statement.
    ///
    /// `create_table_query` is the fallback and not the answer: it is the same
    /// statement with the `CREATE VIEW` header still attached, which is what
    /// SQLite's driver returns anyway. A non-view's `create_table_query` is a
    /// real thing somebody wants, but it belongs on a DDL call — this one's
    /// contract is "None for a relation that has none", and the structure pane
    /// hangs a whole section on the distinction.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, ChError> {
        let rows = self
            .client
            .query(
                "SELECT engine, as_select, create_table_query FROM system.tables \
                 WHERE database = ? AND name = ?",
            )
            .bind(schema)
            .bind(relation)
            .fetch_all::<DefinitionRow>()
            .await
            .map_err(|e| ChError::from_server(e, None))?;

        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        if !matches!(
            relation_kind(&row.engine),
            RelationKind::View | RelationKind::MaterializedView
        ) {
            return Ok(None);
        }
        Ok(some_unless_empty(row.as_select).or_else(|| some_unless_empty(row.create_table_query)))
    }

    /// Data skipping indexes. Not the sorting key — see `Storage::sorting_key`
    /// for why that is a property of the relation instead.
    pub async fn indexes(&self, schema: &str, relation: &str) -> Result<Vec<IndexInfo>, ChError> {
        let rows = self
            .client
            .query(
                "SELECT name, type_full, expr, granularity \
                 FROM system.data_skipping_indices \
                 WHERE database = ? AND table = ? ORDER BY name",
            )
            .bind(schema)
            .bind(relation)
            .fetch_all::<IndexRow>()
            .await
            .map_err(|e| ChError::from_server(e, None))?;

        Ok(rows
            .into_iter()
            .map(|r| IndexInfo {
                name: r.name,
                // One entry, and an expression rather than a column name,
                // because a skip index over `lower(payload)` is not one over
                // `payload` — which is the same reason the shared struct holds
                // expressions in the first place.
                columns: vec![r.expr],
                // Neither is a thing a skip index can be. It does not identify a
                // row; it lets the planner discard a granule without reading it.
                is_unique: false,
                is_primary: false,
                // `type_full` and not `type`: the arguments are what make the
                // index's behaviour readable — `set(100)` and
                // `ngrambf_v1(3, 256, 2, 0)` say something `set` and `ngrambf_v1`
                // do not. GRANULARITY rides along because `IndexInfo` has no
                // field for it and without it the selectivity is unreadable;
                // together they are the DDL spelling of the index, which is the
                // form somebody would recognise.
                method: format!("{} GRANULARITY {}", r.type_full, r.granularity),
                // A skip index has no `WHERE`. `type_full` is where its
                // parameters are.
                predicate: None,
            })
            .collect())
    }

    /// Empty, always, and without asking: ClickHouse declares no foreign keys.
    pub async fn foreign_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, ChError> {
        Ok(Vec::new())
    }

    /// Empty for the same reason as `foreign_keys`: nothing is declared to look
    /// up from the other end either.
    ///
    /// There *is* a real dependency graph — `system.tables.dependencies_table`
    /// lists the materialized views reading from a table, which is genuinely
    /// "what depends on this". It must not be returned here. `RelationshipInfo`
    /// is `local_columns`, `other_columns`, `on_update`, `on_delete`: four
    /// fields that would all be empty or invented, describing a relationship
    /// with no columns and no referential actions. If the structure pane wants
    /// it, it gets a `dependents()` call of its own.
    pub async fn referenced_by(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, ChError> {
        Ok(Vec::new())
    }

    /// Empty. ClickHouse has `CHECK` constraints and no catalog to read them
    /// from — see the module comment.
    pub async fn constraints(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<ConstraintInfo>, ChError> {
        Ok(Vec::new())
    }

    /// Empty, always. ClickHouse has no triggers.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, ChError> {
        Ok(Vec::new())
    }
}

/// What a column does when it is left out of an `INSERT`.
///
/// `ColumnInfo` has one field and ClickHouse has four answers. `DEFAULT` is the
/// one that means what the field is named after; `MATERIALIZED` is computed on
/// insert and cannot be written explicitly, `ALIAS` is not stored at all and
/// `SELECT *` does not return it, `EPHEMERAL` exists only for the duration of
/// the insert. Reporting any of the three as a plain default would tell the
/// reader they may write to a column they may not.
///
/// So the kind is kept in front of the expression, which is how the DDL spells
/// it and how upstream's generated DDL shows it. `MATERIALIZED id * 2` in a
/// Default column cannot be misread as a default; an unlabelled `id * 2` can.
fn default_of(kind: &str, expression: String) -> Option<String> {
    let expression = some_unless_empty(expression)?;
    match kind {
        "DEFAULT" | "" => Some(expression),
        other => Some(format!("{other} {expression}")),
    }
}

/// The ClickHouse-only facts about one table.
pub(crate) async fn storage(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Option<Storage>, ChError> {
    let rows = client
        .query(
            "SELECT engine, sorting_key, primary_key, partition_key, comment \
             FROM system.tables WHERE database = ? AND name = ?",
        )
        .bind(schema)
        .bind(relation)
        .fetch_all::<StorageRow>()
        .await
        .map_err(|e| ChError::from_server(e, None))?;

    Ok(rows.into_iter().next().map(|r| Storage {
        engine: r.engine,
        sorting_key: some_unless_empty(r.sorting_key),
        primary_key: some_unless_empty(r.primary_key),
        partition_key: some_unless_empty(r.partition_key),
        comment: some_unless_empty(r.comment),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A materialized view holds data and a plain view does not, so a structure
    /// pane that offered the same actions for both would be wrong about one of
    /// them.
    #[test]
    fn the_two_kinds_of_view_are_not_the_same_kind() {
        assert_eq!(relation_kind("View"), RelationKind::View);
        assert_eq!(
            relation_kind("MaterializedView"),
            RelationKind::MaterializedView
        );
        assert_eq!(relation_kind("MergeTree"), RelationKind::Table);
        assert_eq!(relation_kind("ReplacingMergeTree"), RelationKind::Table);
        assert_eq!(relation_kind("Log"), RelationKind::Table);
        assert_eq!(relation_kind("Dictionary"), RelationKind::Virtual);
    }

    #[test]
    fn only_a_default_is_reported_as_one() {
        assert_eq!(
            default_of("DEFAULT", "'none'".to_string()).as_deref(),
            Some("'none'")
        );
        assert_eq!(
            default_of("MATERIALIZED", "id * 2".to_string()).as_deref(),
            Some("MATERIALIZED id * 2"),
            "an expression a user cannot write to must not read as one they can"
        );
        assert_eq!(
            default_of("ALIAS", "id + 1".to_string()).as_deref(),
            Some("ALIAS id + 1")
        );
        assert_eq!(default_of("", String::new()), None);
    }
}
