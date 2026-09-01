//! ClickHouse, read out of `ext.clickhouse`.
//!
//! One statement out and one string in, which is SQLite's shape rather than
//! PostgreSQL's: `ClickhouseMetaModel.getTableDDL` runs `SHOW CREATE TABLE` and
//! prints back what the server answers with. `getViewDDL` is one line that calls
//! it, so a view takes the same path — and so does a materialized view, which is
//! what `SHOW CREATE TABLE` is named after rather than in spite of.
//!
//! Two differences from upstream, both deliberate:
//!
//! - `normalizeDDL` is not reproduced. It re-breaks the statement after every
//!   comma, which was right for a server that answered on one line; ClickHouse 24
//!   pretty-prints the statement itself, so running it now puts a newline inside
//!   every `Decimal(9, 4)` and `DateTime('Asia/Taipei')`. Reproducing it would
//!   mean shipping a defect to match one.
//! - Upstream sends the `system` schema down the generic catalog-built path
//!   instead. There is no generic builder here, and the server answers `SHOW
//!   CREATE TABLE` for its own tables perfectly well, so they are read like any
//!   others.

use crate::{ColumnKind, DatabaseChange, Renderer, TableChange, create_table_text};
use arrow::array::{Array, StringArray};
use arrow::datatypes::Schema;
use async_trait::async_trait;
use dbconn::{DbError, DbResult, Driver, RelationInfo};

pub(crate) static CLICKHOUSE: Clickhouse = Clickhouse;

pub(crate) struct Clickhouse;

#[async_trait]
impl Renderer for Clickhouse {
    /// Every kind, because `SHOW CREATE TABLE` answers for every kind.
    ///
    /// No arm refuses, where the SQLite and MySQL renderers have one: those two
    /// have to know whether they are looking at a view before they can ask the
    /// right question, and this one asks the same question either way. A kind
    /// ClickHouse does not have therefore reaches the server, which says so
    /// better than a guess here could.
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
        let name = qualified(&relation.schema, &relation.name);
        let mut stream = driver
            .query(&format!("SHOW CREATE TABLE {name}"), ROWS_PER_BATCH)
            .await?;

        let mut statements = Vec::new();
        while let Some(batch) = stream.next_batch().await? {
            let column = batch.column(0);
            let text = column
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    DbError::new(format!(
                        "the statement arrived as {} rather than as text",
                        column.data_type()
                    ))
                })?;
            statements.extend(
                (0..text.len())
                    .filter(|row| !text.is_null(*row))
                    .map(|row| text.value(row).to_string()),
            );
        }

        if statements.is_empty() {
            // Upstream returns the empty string it accumulated and shows a blank
            // tab. A relation the navigator listed and the server will not
            // describe means the tree has gone stale, and saying so beats a pane
            // that explains nothing.
            return Err(DbError::new(format!(
                "{name} is listed but the server has no statement for it"
            )));
        }
        // Upstream appends a newline to each row and trims nothing;
        // `SQLSourceViewer.getSourceText` trims the tail before it is shown.
        // Joined and trimmed here for the reason [`crate::Script::finish`] gives:
        // a caller writing this to a file or a test assertion should get the
        // same string as one showing it.
        Ok(statements.join("\n").trim_end().to_string())
    }

    /// ClickHouse's words for the kinds a file can ask for.
    ///
    /// Every column `Nullable`, and an engine after the bracket. ClickHouse has
    /// neither by default: a plain `Int64` column refuses a NULL rather than
    /// storing one, and a table with no engine is not a table it will make. The
    /// engine is `MergeTree` ordered by nothing, which is the shape that makes no
    /// claim about the data — a sort key guessed from a file would be a
    /// performance decision taken on somebody's behalf and written into the
    /// table's definition.
    fn create_table(&self, table: &str, columns: &Schema) -> DbResult<String> {
        create_table_text(
            &dbsql::CLICKHOUSE,
            table,
            columns,
            word,
            "\nENGINE = MergeTree\nORDER BY tuple()",
        )
    }

    /// None of the three yet.
    ///
    /// ClickHouse has all of them — `DROP TABLE`, `TRUNCATE TABLE`,
    /// `RENAME TABLE … TO …` — and upstream writes none of them: `ext.clickhouse`
    /// has no `addObjectRenameActions` and no truncate tool, so its tables are
    /// dropped through the generic manager and renamed not at all. A statement
    /// written here would be this build's guess rather than a reading of the
    /// specification the rest of this file is written against, and the families
    /// are lit one at a time.
    fn table_change(&self, _relation: &RelationInfo, _change: TableChange<'_>) -> DbResult<String> {
        Err(DbError::new(
            "changing a table has not been written for ClickHouse yet",
        ))
    }

    /// None are written, so the items are not drawn at all.
    fn changes_relations(&self) -> bool {
        false
    }

    /// Neither is written yet, for the reason the relation changes are not:
    /// upstream is the specification and the families are lit one at a time.
    fn database_change(&self, _change: DatabaseChange<'_>) -> DbResult<String> {
        Err(DbError::new(
            "making or removing a database has not been written for ClickHouse yet",
        ))
    }

    /// Neither is written, so the items are not drawn at all.
    fn changes_databases(&self) -> bool {
        false
    }
}

/// A statement is one row, so this decides only how large a buffer holds it.
const ROWS_PER_BATCH: usize = 64;

/// `schema.name`, quoted the way ClickHouse reads it.
fn qualified(schema: &str, name: &str) -> String {
    format!(
        "{}.{}",
        dbsql::CLICKHOUSE.quote(schema),
        dbsql::CLICKHOUSE.quote(name)
    )
}

fn word(kind: ColumnKind) -> String {
    let inner = match kind {
        ColumnKind::Bool => "Bool".to_string(),
        ColumnKind::Int => "Int64".to_string(),
        ColumnKind::Float => "Float64".to_string(),
        ColumnKind::Decimal(precision, scale) => format!("Decimal({precision}, {scale})"),
        ColumnKind::Text => "String".to_string(),
        ColumnKind::Date => "Date32".to_string(),
        // Microseconds, which is the precision the inference asks for and the
        // most a `DateTime64` can be given without losing years at either end.
        ColumnKind::Timestamp => "DateTime64(6)".to_string(),
    };
    format!("Nullable({inner})")
}
