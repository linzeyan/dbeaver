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
