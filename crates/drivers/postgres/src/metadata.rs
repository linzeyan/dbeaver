//! Schema introspection for the navigator sidebar.
//!
//! Queries `pg_catalog` directly rather than `information_schema`. The latter
//! is portable but implemented as views over the former, and is markedly slower
//! on databases with many objects — which is exactly where a sidebar has to
//! stay responsive.
//!
//! Unlike result data, metadata is small and crosses the FFI boundary as JSON.
//! Arrow buys nothing for a few thousand short rows, and JSON keeps the Swift
//! side trivial.

use serde::Serialize;
use tokio_postgres::Client;

use crate::PgError;

#[derive(Debug, Clone, Serialize)]
pub struct SchemaInfo {
    pub name: String,
}

/// What kind of relation a navigator entry is. Views and tables look the same
/// when browsing data but differ in what may be done to them, so the
/// distinction is carried from the start rather than retrofitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    PartitionedTable,
    Unknown,
}

impl RelationKind {
    fn from_relkind(c: i8) -> Self {
        match c as u8 as char {
            'r' => Self::Table,
            'v' => Self::View,
            'm' => Self::MaterializedView,
            'f' => Self::ForeignTable,
            'p' => Self::PartitionedTable,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationInfo {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    /// Planner estimate. Exact counts require a scan, which is not acceptable
    /// for a sidebar; the estimate is labelled as such in the UI.
    pub estimated_rows: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    /// Fully formatted SQL type, e.g. `numeric(18,4)` or `character varying(64)`.
    pub data_type: String,
    pub nullable: bool,
    pub position: i32,
    pub is_primary_key: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    pub name: String,
    /// Key expressions in index order. Expressions rather than plain names,
    /// because an index on `lower(email)` is not an index on `email` and
    /// printing it as one would be a lie about what the planner can use.
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    /// Access method: btree, hash, gin, gist, brin.
    pub method: String,
    /// WHERE clause of a partial index, if any.
    pub predicate: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

/// `confupdtype`/`confdeltype` spelled the way the DDL spells them.
fn referential_action(c: i8) -> String {
    match c as u8 as char {
        'r' => "RESTRICT",
        'c' => "CASCADE",
        'n' => "SET NULL",
        'd' => "SET DEFAULT",
        // 'a' is the default, and writing it out on every row is noise.
        _ => "NO ACTION",
    }
    .to_string()
}

pub(crate) async fn schemas(client: &Client) -> Result<Vec<SchemaInfo>, PgError> {
    // Excludes catalog and toast schemas; `pg_temp`/`pg_toast_temp` are matched
    // by the same prefix.
    let rows = client
        .query(
            "SELECT nspname \
             FROM pg_catalog.pg_namespace \
             WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
             ORDER BY nspname",
            &[],
        )
        .await?;
    Ok(rows.iter().map(|r| SchemaInfo { name: r.get(0) }).collect())
}

pub(crate) async fn relations(client: &Client, schema: &str) -> Result<Vec<RelationInfo>, PgError> {
    let rows = client
        .query(
            "SELECT c.relname, c.relkind, c.reltuples::bigint \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'v', 'm', 'f', 'p') \
             ORDER BY c.relname",
            &[&schema],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let estimated: i64 = r.get(2);
            RelationInfo {
                schema: schema.to_string(),
                name: r.get(0),
                kind: RelationKind::from_relkind(r.get(1)),
                // reltuples is -1 when the relation has never been analyzed.
                estimated_rows: estimated.max(0),
            }
        })
        .collect())
}

pub(crate) async fn columns(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<ColumnInfo>, PgError> {
    let rows = client
        .query(
            "SELECT a.attname, \
                    pg_catalog.format_type(a.atttypid, a.atttypmod), \
                    NOT a.attnotnull, \
                    a.attnum::int, \
                    COALESCE(pk.indisprimary, false), \
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_catalog.pg_attrdef d \
                    ON d.adrelid = c.oid AND d.adnum = a.attnum \
             LEFT JOIN pg_catalog.pg_index pk \
                    ON pk.indrelid = c.oid AND pk.indisprimary \
                   AND a.attnum = ANY(pk.indkey) \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&schema, &relation],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            name: r.get(0),
            data_type: r.get(1),
            nullable: r.get(2),
            position: r.get(3),
            is_primary_key: r.get(4),
            default_value: r.get(5),
        })
        .collect())
}

pub(crate) async fn indexes(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<IndexInfo>, PgError> {
    // Key expressions come from pg_get_indexdef one position at a time rather
    // than by joining indkey against pg_attribute: that join silently drops
    // expression keys, which appear in indkey as attnum 0.
    let rows = client
        .query(
            "SELECT i.relname, \
                    ix.indisunique, \
                    ix.indisprimary, \
                    am.amname, \
                    pg_catalog.pg_get_expr(ix.indpred, ix.indrelid), \
                    ARRAY(SELECT pg_catalog.pg_get_indexdef(ix.indexrelid, k::int, true) \
                          FROM generate_series(1, ix.indnkeyatts) AS k \
                          ORDER BY k) \
             FROM pg_catalog.pg_index ix \
             JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_catalog.pg_class c ON c.oid = ix.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_am am ON am.oid = i.relam \
             WHERE n.nspname = $1 AND c.relname = $2 \
             ORDER BY ix.indisprimary DESC, i.relname",
            &[&schema, &relation],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| IndexInfo {
            name: r.get(0),
            is_unique: r.get(1),
            is_primary: r.get(2),
            method: r.get(3),
            predicate: r.get(4),
            columns: r.get(5),
        })
        .collect())
}

pub(crate) async fn foreign_keys(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<ForeignKeyInfo>, PgError> {
    // WITH ORDINALITY on both key arrays: a composite key's columns have to
    // line up with the ones they reference, and attnum order is not that order.
    let rows = client
        .query(
            "SELECT con.conname, \
                    ARRAY(SELECT a.attname \
                          FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
                          JOIN pg_catalog.pg_attribute a \
                            ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord), \
                    fn.nspname, \
                    f.relname, \
                    ARRAY(SELECT a.attname \
                          FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord) \
                          JOIN pg_catalog.pg_attribute a \
                            ON a.attrelid = con.confrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord), \
                    con.confupdtype, \
                    con.confdeltype \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_class f ON f.oid = con.confrelid \
             JOIN pg_catalog.pg_namespace fn ON fn.oid = f.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND con.contype = 'f' \
             ORDER BY con.conname",
            &[&schema, &relation],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| ForeignKeyInfo {
            name: r.get(0),
            columns: r.get(1),
            referenced_schema: r.get(2),
            referenced_table: r.get(3),
            referenced_columns: r.get(4),
            on_update: referential_action(r.get(5)),
            on_delete: referential_action(r.get(6)),
        })
        .collect())
}
