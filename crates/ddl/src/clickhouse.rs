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

use crate::{
    ColumnChange, ColumnKind, DatabaseChange, NewColumn, NullStyle, Renderer, TableChange,
    new_table_text,
};
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

    /// ClickHouse's words for the kinds a new table can ask for, plus an engine
    /// after the bracket.
    ///
    /// A table with no engine is not a table ClickHouse will make. The engine is
    /// `MergeTree` ordered by nothing, which is the shape that makes no claim
    /// about the data — a sort key guessed from a file would be a performance
    /// decision taken on somebody's behalf and written into the table's
    /// definition.
    ///
    /// Which is also why a primary key is refused rather than written. On a
    /// `MergeTree` the primary key is the sort order, and ClickHouse insists it
    /// be a prefix of `ORDER BY`; a `PRIMARY KEY (id)` under `ORDER BY tuple()`
    /// is rejected outright. Honouring the checkbox would mean choosing the
    /// table's physical order from a form that never mentions ordering, so the
    /// answer is the refusal and the caller keeps the choice.
    fn new_table(&self, table: &str, columns: &[NewColumn]) -> DbResult<String> {
        if columns.iter().any(|column| column.primary_key) {
            return Err(DbError::new(
                "a ClickHouse table is stored in the order of its primary key, which is a \
                 choice this form does not offer",
            ));
        }
        new_table_text(
            &dbsql::CLICKHOUSE,
            table,
            columns,
            word,
            NullStyle::Wrapped,
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

    /// None of the three yet, for the reason the relation changes are not:
    /// upstream is the specification and the families are lit one at a time. The
    /// statements exist on this server — a column here is added, dropped and
    /// renamed like anywhere else — so this is a refusal about what has been
    /// written rather than about what ClickHouse can do.
    fn column_change(
        &self,
        _relation: &RelationInfo,
        _change: ColumnChange<'_>,
    ) -> DbResult<String> {
        Err(DbError::new(
            "changing a column has not been written for ClickHouse yet",
        ))
    }

    /// None are written, so the controls are not drawn at all.
    fn changes_columns(&self) -> bool {
        false
    }

    /// Not written either, and the two are asked separately because they are
    /// separate questions everywhere else: SQLite changes the set of columns and
    /// alters none of them.
    fn alters_columns(&self) -> bool {
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

/// The type alone, with nullability left to [`NullStyle::Wrapped`].
///
/// ClickHouse spells it inside the type rather than after it — `Nullable(Int64)`
/// against a bare `Int64` that refuses a NULL — and the wrapping lives with the
/// rest of the column layout so that one place decides whether a column takes
/// one.
fn word(kind: ColumnKind) -> String {
    match kind {
        ColumnKind::Bool => "Bool".to_string(),
        ColumnKind::Int => "Int64".to_string(),
        ColumnKind::Float => "Float64".to_string(),
        ColumnKind::Decimal(precision, scale) => format!("Decimal({precision}, {scale})"),
        ColumnKind::Text => "String".to_string(),
        ColumnKind::Date => "Date32".to_string(),
        // Microseconds, which is the precision the inference asks for and the
        // most a `DateTime64` can be given without losing years at either end.
        ColumnKind::Timestamp => "DateTime64(6)".to_string(),
    }
}
