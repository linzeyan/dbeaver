//! DuckDB, read out of `ext.duckdb`.
//!
//! DuckDB keeps the statement each object was created from, so this renderer has
//! SQLite's shape rather than PostgreSQL's: one lookup, one string, nothing
//! assembled. `DuckMetaModel.getTableDDL` reads `duckdb_tables().sql` and
//! `getViewDDL` reads `duckdb_views().sql`, both through one private helper that
//! filters on the database, the schema and the object's name.
//!
//! Nothing else goes into a table's DDL — no `DROP` header, no index section, no
//! triggers. `DuckMetaModel` overrides `getTableDDL` outright, so the shared
//! `SQLTableManager.getTableDDL` that supplies those to PostgreSQL never runs,
//! and `getObjectDDL` returns the one row it found.
//!
//! Two deliberate differences from upstream:
//!
//! - `SQLFormatUtils.formatSQL` is not reproduced, for the reason the PostgreSQL
//!   renderer gives about a trigger: there is no formatter here, and the text
//!   DuckDB stored is already a statement somebody wrote.
//! - Where the lookup finds nothing, upstream falls back to building a table's
//!   DDL out of the catalog and prints `-- DuckDB view definition not found` for
//!   a view. This crate has no generic builder and does not write comments in
//!   place of answers, so both are refused with a message naming the object.

use crate::{
    ColumnKind, DatabaseChange, NewColumn, NullStyle, Renderer, TableChange, new_table_text,
};
use arrow::array::{Array, StringArray};
use async_trait::async_trait;
use dbconn::{DbError, DbResult, Driver, RelationInfo, RelationKind};

pub(crate) static DUCKDB: DuckDb = DuckDb;

pub(crate) struct DuckDb;

#[async_trait]
impl Renderer for DuckDb {
    /// Tables and views, and a refusal for the rest.
    ///
    /// Upstream has the same two and no more: `DuckMetaModel` overrides
    /// `getTableDDL` and `getViewDDL`, and a kind that is neither would reach a
    /// generic builder this crate does not have.
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
        match relation.kind {
            RelationKind::Table => table(driver, relation).await,
            RelationKind::View => view(driver, relation).await,
            kind => Err(DbError::new(format!(
                "{} is a {kind:?}, and DuckDB has no such object",
                qualified(&relation.schema, &relation.name)
            ))),
        }
    }

    /// DuckDB's words for the kinds a file can ask for.
    fn new_table(&self, table: &str, columns: &[NewColumn]) -> DbResult<String> {
        new_table_text(&dbsql::DUCKDB, table, columns, word, NullStyle::Suffix, "")
    }

    /// None of the three yet, for the reason ClickHouse's says: `ext.duckdb`
    /// overrides no rename action, so upstream has nothing to be read here.
    fn table_change(&self, _relation: &RelationInfo, _change: TableChange<'_>) -> DbResult<String> {
        Err(DbError::new(
            "changing a table has not been written for DuckDB yet",
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
            "making or removing a database has not been written for DuckDB yet",
        ))
    }

    /// Neither is written, so the items are not drawn at all.
    fn changes_databases(&self) -> bool {
        false
    }
}

/// A table, from `duckdb_tables()`.
///
/// Upstream filters `database_name = ? AND schema_name = ?` and so does this,
/// with `current_database()` for the half a `RelationInfo` does not carry:
/// `Driver::schemas` lists the schemas of the database the session is on, so a
/// relation's schema is `main` and which `main` it is comes from the session.
/// `Driver::query` runs on that session connection — the same one `use_database`
/// sends its `USE` to — so the two always name the same database. Asking DuckDB
/// rather than threading the name through the trait keeps the renderer working
/// off the driver it was handed and nothing else.
async fn table(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let schema = relation.schema.as_str();
    let name = relation.name.as_str();
    let statement = format!(
        "SELECT sql FROM duckdb_tables() \
         WHERE database_name = current_database() AND schema_name = {} AND table_name = {}",
        literal(schema),
        literal(name)
    );

    // One row is the whole result, so the batch size only decides the size of
    // the buffer that holds it.
    let mut stream = driver.query(&statement, 1).await?;
    let found = match stream.next_batch().await? {
        Some(batch) => {
            let column = batch.column(0);
            let sql = column
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    DbError::new(format!(
                        "duckdb_tables.sql arrived as {} rather than as text",
                        column.data_type()
                    ))
                })?;
            // A NULL statement and no row at all are the same answer to the
            // caller — upstream's `isNotEmpty` test treats them alike — so both
            // fall through to the refusal below.
            (batch.num_rows() > 0 && !sql.is_null(0)).then(|| sql.value(0).to_string())
        }
        None => None,
    };

    found.map(trimmed).ok_or_else(|| {
        DbError::new(format!(
            "{} is listed as a table but duckdb_tables() has no statement for it",
            qualified(schema, name)
        ))
    })
}

/// A view, which is the statement it was created from and nothing else.
///
/// `Driver::definition` already runs upstream's query: `driver-duckdb` reads
/// `duckdb_views().sql` for it, filtered the same way, because that is also what
/// the structure pane shows in a view's Source section.
async fn view(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let statement = driver
        .definition(&relation.schema, &relation.name)
        .await?
        .ok_or_else(|| {
            DbError::new(format!(
                "{} is listed as a view but duckdb_views() has no statement for it",
                qualified(&relation.schema, &relation.name)
            ))
        })?;
    Ok(trimmed(statement))
}

/// The statement without the whitespace DuckDB stored after it.
///
/// No semicolon is added, where the SQLite renderer adds one: that comes from
/// `SQLiteUtils.readMasterDefinition` appending it, and `DuckMetaModel` has no
/// equivalent — it returns the column as it found it. DuckDB stores the
/// terminator inside the statement anyway.
fn trimmed(statement: String) -> String {
    statement.trim_end().to_string()
}

/// A string DuckDB reads as text.
///
/// Upstream binds these as parameters and this cannot: `Driver::query` takes a
/// statement and nothing beside it. Doubling the quote is the whole of the
/// escaping needed, DuckDB having no backslash escape inside an ordinary string
/// — which is what `dbsql::DUCKDB` records as `backslash_escapes: false`.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `schema.name`, for a message that has to say which object it is about.
fn qualified(schema: &str, name: &str) -> String {
    format!(
        "{}.{}",
        dbsql::DUCKDB.quote(schema),
        dbsql::DUCKDB.quote(name)
    )
}

fn word(kind: ColumnKind) -> String {
    match kind {
        ColumnKind::Bool => "BOOLEAN".to_string(),
        ColumnKind::Int => "BIGINT".to_string(),
        ColumnKind::Float => "DOUBLE".to_string(),
        ColumnKind::Decimal(precision, scale) => format!("DECIMAL({precision}, {scale})"),
        ColumnKind::Text => "VARCHAR".to_string(),
        ColumnKind::Date => "DATE".to_string(),
        ColumnKind::Timestamp => "TIMESTAMP".to_string(),
    }
}
