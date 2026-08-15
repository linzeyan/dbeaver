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

use crate::Renderer;
use arrow::array::{Array, StringArray};
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
