//! SQLite, read out of `ext.sqlite`.
//!
//! A different shape of renderer from PostgreSQL's, because SQLite keeps the
//! statement each object was created from. `sqlite_master.sql` holds the text
//! that was typed, and upstream prints it back rather than rebuilding anything
//! out of a catalog. The path for a table is
//! `GenericTable.getObjectDefinitionText` → `SQLiteMetaModel.getTableDDL` →
//! `SQLiteUtils.readMasterDefinition`, called twice — once for the table and once
//! for the indexes on it — and for a view it is
//! `GenericView.getObjectDefinitionText` → `SQLiteMetaModel.getViewDDL` → the
//! same function. Those are what this file reproduces.
//!
//! The DDL here does not go through [`crate::Script`], which is not an
//! oversight: `Script` reproduces `SQLUtils.generateScript`, and the path a
//! table's DDL takes never reaches it. `readMasterDefinition` does its own
//! joining — every statement followed by a semicolon and a newline — and
//! `getTableDDL` puts one blank line between the table and its indexes. Those
//! two rules are the whole of the arrangement, and borrowing PostgreSQL's would
//! produce a script upstream does not write.
//!
//! [`Renderer::table_change`] is the exception, and for the reason the rule has:
//! `SQLiteTableManager` inherits `addObjectDeleteActions` and overrides
//! `addObjectRenameActions`, both of which are the shared editor path that ends
//! in `SQLUtils.generateScript`. Same crate, different upstream route.
//!
//! Two things absent from a table's DDL here that PostgreSQL's has, stated
//! because their absence looks like a gap. There is no commented-out `DROP
//! TABLE`: that header belongs to `SQLTableManager.getTableDDL`, which
//! `SQLiteMetaModel` overrides away entirely. And there are no triggers:
//! `SQLiteMetaModel.getTriggerDDL` renders a trigger when the navigator asks
//! about that trigger, and a table's DDL never calls it, where
//! `PostgreTableManagerBase.addObjectExtraActions` appends them.
//!
//! The options that decide what PostgreSQL emits decide nothing here.
//! `getTableDDL` and `getViewDDL` take the map and never read it, so there is no
//! preference whose default had to be established before this could be written.

use crate::{
    ColumnKind, DatabaseChange, NewColumn, NullStyle, Renderer, Script, TableChange, new_table_text,
};
use arrow::array::{Array, StringArray};
use async_trait::async_trait;
use dbconn::{DbError, DbResult, Driver, RelationInfo, RelationKind};

pub(crate) static SQLITE: Sqlite = Sqlite;

pub(crate) struct Sqlite;

#[async_trait]
impl Renderer for Sqlite {
    /// Tables, virtual tables and views, and a refusal for the rest.
    ///
    /// A virtual table takes the table path because upstream cannot tell the two
    /// apart and does not try: `sqlite_master` files `CREATE VIRTUAL TABLE …`
    /// under `type='table'`, so `readMasterDefinition` finds it with the filter
    /// it already has, and `SQLiteMetaModel.createTableOrViewImpl` builds a
    /// `SQLiteTable` for it. The kinds reaching the last arm are ones SQLite does
    /// not have at all, so a relation claiming to be one did not come from this
    /// database and there is nothing to print.
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
        match relation.kind {
            RelationKind::Table | RelationKind::Virtual => table(driver, relation).await,
            RelationKind::View => view(driver, relation).await,
            kind => Err(DbError::new(format!(
                "{} is a {kind:?}, and SQLite has no such object",
                qualified(&relation.schema, &relation.name)
            ))),
        }
    }

    /// SQLite's declared type names for the kinds a file can ask for.
    ///
    /// Declared names and not storage classes: SQLite decides what a value is by
    /// looking at the value, and a column's type only sets which conversion it
    /// tries first. What is written here is therefore for whoever reads the
    /// table back, and is chosen to match what will actually be in it.
    fn new_table(&self, table: &str, columns: &[NewColumn]) -> DbResult<String> {
        new_table_text(&dbsql::SQLITE, table, columns, word, NullStyle::Suffix, "")
    }

    /// Two of the three. SQLite has no `TRUNCATE` at all.
    fn table_change(&self, relation: &RelationInfo, change: TableChange<'_>) -> DbResult<String> {
        let name = qualified(&relation.schema, &relation.name);
        let noun = match relation.kind {
            // A virtual table drops as a table, which is how it was made:
            // `CREATE VIRTUAL TABLE` has no `DROP VIRTUAL TABLE` to match it.
            RelationKind::Table | RelationKind::Virtual => "TABLE",
            RelationKind::View => "VIEW",
            kind => {
                return Err(DbError::new(format!(
                    "SQLite has no {kind:?}, so there is no statement to write for one"
                )));
            }
        };
        match change {
            TableChange::Drop => Ok(crate::drop_text(noun, &name)),
            // Not `DELETE FROM`, which is what SQLite offers instead and is not
            // the same statement: it fires triggers, it can be rolled back, and
            // it leaves the rowid counter where it was. Offering it under the
            // word `Truncate` would be answering a question nobody asked, so the
            // refusal says what SQLite does have and lets somebody type it.
            TableChange::Truncate => Err(DbError::new(format!(
                "SQLite has no TRUNCATE; emptying {name} is `DELETE FROM {name}`, which is a \
                 different statement — it fires triggers and can be rolled back"
            ))),
            // `SQLiteTableManager.addObjectRenameActions`: the old name
            // qualified by its schema, the new one bare.
            //
            // A table only. SQLite's `ALTER TABLE` reaches nothing else, and
            // renaming a view means dropping it and creating it again under
            // another name — two statements, one of which loses the definition
            // if the second fails.
            TableChange::Rename { to } => {
                if noun != "TABLE" {
                    return Err(DbError::new(format!(
                        "SQLite cannot rename a view; {name} would have to be dropped and \
                         created again under the new name"
                    )));
                }
                let mut script = Script::new();
                script.statement(&format!(
                    "ALTER TABLE {name} RENAME TO {}",
                    dbsql::SQLITE.quote(to)
                ));
                Ok(script.finish())
            }
        }
    }

    /// Two of the three are written above, which is enough for the items to be
    /// worth drawing; the third refuses where its statement would have been.
    fn changes_relations(&self) -> bool {
        true
    }

    /// Neither, and not because nobody has written them.
    ///
    /// A SQLite database is a file. It is made by opening a path that has none
    /// and removed by deleting one, and the nearest statement — `ATTACH` — puts
    /// a second file on this connection rather than making anything. There is no
    /// `CREATE DATABASE` here to teach, which is why this refusal names the
    /// filesystem instead of promising a later version.
    fn database_change(&self, _: DatabaseChange<'_>) -> DbResult<String> {
        Err(DbError::new(
            "a SQLite database is a file, made and removed on the filesystem rather than by SQL",
        ))
    }

    fn changes_databases(&self) -> bool {
        false
    }
}

/// A table, as `SQLiteMetaModel.getTableDDL` assembles one.
///
/// The statement the table was created from, then every index on it, with one
/// blank line between them: that method returns `tableDDL + "\n" + indexesDDL`
/// and both halves already end in a newline of their own.
async fn table(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let schema = relation.schema.as_str();
    let name = relation.name.as_str();

    let created = master_definitions(driver, schema, "table", name, Some(name)).await?;
    if created.is_empty() {
        // Upstream shows an empty DDL tab here: `readMasterDefinition`
        // accumulates nothing when the query matches no row, and `getTableDDL`
        // hands that empty string straight back. A relation the navigator listed
        // and `sqlite_master` has never heard of means the tree has gone stale,
        // and saying so beats a blank pane that explains nothing.
        return Err(DbError::new(format!(
            "{} is listed as a table but sqlite_master has no statement for it",
            qualified(schema, name)
        )));
    }
    let indexes = master_definitions(driver, schema, "index", name, None).await?;

    let mut ddl = terminated(&created);
    // Emptiness is tested after the rows have been turned into text, not before,
    // which is `CommonUtils.isEmpty(indexesDDL)` and matters for a table whose
    // only index SQLite built for it: `sqlite_autoindex_…` is a row with no
    // statement, so there are index rows and no index text, and the blank line
    // must not appear with nothing under it.
    if !indexes.is_empty() {
        ddl.push('\n');
        ddl.push_str(&terminated(&indexes));
    }
    Ok(trimmed(ddl))
}

/// A view, which is the statement it was created from and nothing else.
///
/// `SQLiteMetaModel.getViewDDL` is one `readMasterDefinition` call with nothing
/// after it, so unlike PostgreSQL there is no `CREATE OR REPLACE` to write and no
/// body to wrap — upstream even has the rewrite to the replacing form present and
/// commented out. `Driver::definition` already runs this lookup: over
/// `sqlite_schema` rather than the older name for the same table, and filtered on
/// `name` alone where upstream filters on `name` and `tbl_name`, which for a view
/// are the same string.
async fn view(driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
    let statement = driver
        .definition(&relation.schema, &relation.name)
        .await?
        .ok_or_else(|| {
            DbError::new(format!(
                "{} is listed as a view but sqlite_master has no statement for it",
                qualified(&relation.schema, &relation.name)
            ))
        })?;
    Ok(trimmed(terminated(&[statement])))
}

/// `SQLiteUtils.readMasterDefinition`: the `sql` of every `sqlite_master` row of
/// `object_type` belonging to `table`, narrowed to one `name` where one is given.
///
/// A row whose `sql` is NULL is dropped, which is upstream's `if (ddl != null)`.
/// That is what keeps an index SQLite built for a `UNIQUE` or `PRIMARY KEY`
/// clause out of the script: `sqlite_autoindex_…` has no statement of its own,
/// and the clause that caused it is already inside the `CREATE TABLE`.
///
/// No `ORDER BY`, because upstream writes none. The rows arrive in
/// `sqlite_master` order, which is the order the objects were created in, and
/// that is the order the indexes are printed in.
async fn master_definitions(
    driver: &dyn Driver,
    schema: &str,
    object_type: &str,
    table: &str,
    name: Option<&str>,
) -> DbResult<Vec<String>> {
    let mut statement = format!(
        // Upstream qualifies `sqlite_master` only when the object's parent is a
        // `GenericSchema`, and reaches for `sqlite_master` unqualified elsewhere
        // (`SQLiteMetaModel.getFullyQualifiedName` returns the bare name). This
        // qualifies always, because `RelationInfo` carries the schema it was
        // listed under and an unqualified name would read whichever database is
        // attached as `main` instead.
        "SELECT sql FROM {}.sqlite_master WHERE type = {} AND tbl_name = {}",
        dbsql::SQLITE.quote(schema),
        literal(object_type),
        literal(table)
    );
    if let Some(name) = name {
        statement.push_str(&format!(" AND name = {}", literal(name)));
    }
    // Upstream unions `sqlite_temp_master` into the same query, for objects made
    // with `CREATE TEMP`. That union is left out because it could only ever match
    // nothing here: temporary objects belong to the connection that made them,
    // every call through this driver opens its own, and `Driver::schemas` leaves
    // `temp` off the navigator for that reason.

    let mut stream = driver.query(&statement, ROWS_PER_BATCH).await?;
    let mut statements = Vec::new();
    while let Some(batch) = stream.next_batch().await? {
        let column = batch.column(0);
        let sql = column
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                DbError::new(format!(
                    "sqlite_master.sql arrived as {} rather than as text",
                    column.data_type()
                ))
            })?;
        statements.extend(
            (0..sql.len())
                .filter(|row| !sql.is_null(*row))
                .map(|row| sql.value(row).to_string()),
        );
    }
    Ok(statements)
}

/// A relation's own statement and its indexes are a handful of rows, so this
/// decides only whether they arrive in one call or two.
const ROWS_PER_BATCH: usize = 64;

/// Statements as `readMasterDefinition` accumulates them, each terminated and on
/// a line of its own.
///
/// The semicolon goes on unconditionally, as upstream appends it, rather than
/// only where one is missing: SQLite stores a `CREATE` statement without its
/// terminator, so the two rules never disagree on anything this can be handed.
fn terminated(statements: &[String]) -> String {
    statements
        .iter()
        .map(|statement| format!("{statement};\n"))
        .collect()
}

/// The finished script, without the newline the last statement left.
///
/// Upstream keeps it and `SQLSourceViewer.getSourceText` trims it. Trimmed here
/// instead, for the reason [`crate::Script::finish`] gives: a caller writing this
/// to a file, a clipboard or a test assertion should get the same string as one
/// showing it.
fn trimmed(ddl: String) -> String {
    ddl.trim_end().to_string()
}

/// A string SQLite reads as `text`.
///
/// Upstream binds these as parameters and this cannot: `Driver::query` takes a
/// statement and nothing beside it, because a database whose statements are JSON
/// documents has nowhere to put a parameter list. Doubling the quote is the whole
/// of the escaping SQLite needs, having no backslash escape inside an ordinary
/// string — which is what `dbsql::SQLITE` records as `backslash_escapes: false`.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `schema.name`, for a message that has to say which object it is about.
fn qualified(schema: &str, name: &str) -> String {
    format!(
        "{}.{}",
        dbsql::SQLITE.quote(schema),
        dbsql::SQLITE.quote(name)
    )
}

fn word(kind: ColumnKind) -> String {
    match kind {
        ColumnKind::Bool => "BOOLEAN".to_string(),
        ColumnKind::Int => "INTEGER".to_string(),
        ColumnKind::Float => "REAL".to_string(),
        ColumnKind::Decimal(precision, scale) => format!("NUMERIC({precision}, {scale})"),
        ColumnKind::Text => "TEXT".to_string(),
        // SQLite has no date and no timestamp. Rows arrive as quoted strings, so
        // text is what the column will hold whatever it is called; `DATE` would
        // give it numeric affinity, fail to make a number of the string, and
        // store the same text under a name that says otherwise.
        ColumnKind::Date | ColumnKind::Timestamp => "TEXT".to_string(),
    }
}
