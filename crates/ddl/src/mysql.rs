//! MySQL, read out of `ext.mysql`.
//!
//! The server writes this DDL and this crate prints it back, which makes the
//! shape SQLite's rather than PostgreSQL's: one statement out, one string in,
//! nothing assembled from a catalog. `MySQLTableBase.getDDL` runs `SHOW CREATE
//! TABLE` for a table and `SHOW CREATE VIEW` for a view, and reads the column
//! the server names `Create Table` or `Create View`.
//!
//! A table's text is handed on untouched, as upstream hands it on. A view's is
//! not, and that is upstream's doing too: `MySQLView.getAdditionalInfo` cuts the
//! statement at ` VIEW ` and writes a new head in front of it, keeping the
//! algorithm and dropping the `DEFINER` and `SQL SECURITY` clauses that came
//! with it. Which is worth keeping — a `DEFINER` names an account that exists on
//! the server the view came from, and a DDL carrying it is one that fails on any
//! other.
//!
//! Two differences from upstream, both about text this crate cannot produce:
//!
//! - Upstream runs a view's rewritten head through `SQLFormatUtils.formatSQL`,
//!   which is also what tidies the two double spaces its concatenation leaves
//!   behind. There is no formatter here — the same answer [`crate::postgres`]
//!   gives for a trigger — so the spacing is written correctly instead of being
//!   written twice and cleaned up.
//! - `supportsAlterView()` decides upstream's leading keyword and reads a driver
//!   parameter that defaults to false, so `CREATE OR REPLACE` is the branch
//!   MySQL takes. Written as that constant rather than as the test, because the
//!   parameter belongs to a driver descriptor this rewrite has no equivalent of.

use crate::{
    AlterStyle, ColumnChange, ColumnKind, DatabaseChange, NewColumn, NullStyle, Renderer, Script,
    TableChange, new_table_text,
};
use arrow::array::{Array, StringArray};
use async_trait::async_trait;
use dbconn::{DbError, DbResult, Driver, RelationInfo, RelationKind};

pub(crate) static MYSQL: Mysql = Mysql;

pub(crate) struct Mysql;

#[async_trait]
impl Renderer for Mysql {
    /// Tables and views, and a refusal for the rest.
    ///
    /// A partitioned table takes the table path because upstream does not have
    /// the distinction: `isView()` is the whole of its branch, and MySQL keeps a
    /// table's partitioning inside the `CREATE TABLE` it answers with. The kinds
    /// reaching the last arm are ones MySQL does not have, so a relation
    /// claiming to be one did not come from this database.
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String> {
        match relation.kind {
            RelationKind::Table | RelationKind::PartitionedTable => {
                shown(driver, relation, "TABLE", "Create Table").await
            }
            RelationKind::View => {
                let view = shown(driver, relation, "VIEW", "Create View").await?;
                Ok(replacing(&view))
            }
            kind => Err(DbError::new(format!(
                "{} is a {kind:?}, and MySQL has no such object",
                qualified(&relation.schema, &relation.name)
            ))),
        }
    }

    /// MySQL's words for the kinds a file can ask for.
    fn new_table(&self, table: &str, columns: &[NewColumn]) -> DbResult<String> {
        new_table_text(&dbsql::MYSQL, table, columns, word, NullStyle::Suffix, "")
    }

    /// All three. MySQL has two nouns and a rename that names both ends.
    fn table_change(&self, relation: &RelationInfo, change: TableChange<'_>) -> DbResult<String> {
        let name = qualified(&relation.schema, &relation.name);
        let noun = match relation.kind {
            RelationKind::Table | RelationKind::PartitionedTable => "TABLE",
            RelationKind::View => "VIEW",
            kind => {
                return Err(DbError::new(format!(
                    "MySQL has no {kind:?}, so there is no statement to write for one"
                )));
            }
        };
        Ok(match change {
            TableChange::Drop => crate::drop_text(noun, &name),
            TableChange::Truncate => {
                // `MySQLToolTableTruncate`, which writes the statement and
                // nothing else. A table only: MySQL refuses `TRUNCATE` on a view
                // with error 1347, the view not being a base table.
                if noun != "TABLE" {
                    return Err(DbError::new(format!(
                        "{name} is a view, and a view has no rows of its own to remove"
                    )));
                }
                let mut script = Script::new();
                script.statement(&format!("TRUNCATE TABLE {name}"));
                script.finish()
            }
            // `RENAME TABLE old TO new` and not `ALTER TABLE … RENAME TO`.
            // `MySQLTableManager.addObjectRenameActions` chooses between the two
            // on `supportsAlterTableRenameSyntax()`, which is `false` on
            // MySQL itself and overridden only by the forks that need the other
            // spelling — so this is the branch MySQL takes.
            //
            // Both ends qualified, as upstream writes them. `RENAME TABLE` can
            // move a table between databases when they differ, and naming the
            // same schema on both sides is what says this one does not.
            //
            // The word stays `TABLE` for a view, which is not a mistake:
            // `RENAME TABLE` is how MySQL renames a view, there being no
            // `RENAME VIEW`.
            TableChange::Rename { to } => {
                let mut script = Script::new();
                script.statement(&format!(
                    "RENAME TABLE {name} TO {}",
                    qualified(&relation.schema, to)
                ));
                script.finish()
            }
        })
    }

    /// All three are written above.
    fn changes_relations(&self) -> bool {
        true
    }

    /// All three, and the rename is a deliberate departure from upstream.
    ///
    /// `MySQLTableColumnManager.addObjectRenameActions` writes
    /// `ALTER TABLE t CHANGE <old> <the whole declaration again>`, which is how
    /// MySQL renamed a column before 8.0 and the only way it could. Rebuilding
    /// that declaration means restating everything about the column, and
    /// `ColumnInfo` does not carry everything: no character set, no collation,
    /// no `AUTO_INCREMENT`, no comment. A `CHANGE` written from what this build
    /// knows would rename the column and quietly strip whatever it did not —
    /// an `AUTO_INCREMENT` primary key would stop generating keys. `RENAME
    /// COLUMN` changes the name and touches nothing else, and MySQL has had it
    /// since 8.0.
    ///
    /// The same divergence in kind as `table_change`'s materialized-view drop:
    /// upstream's statement is one this build cannot write correctly, so the
    /// smaller statement that does only what was asked is the one written.
    fn column_change(&self, relation: &RelationInfo, change: ColumnChange<'_>) -> DbResult<String> {
        let name = qualified(&relation.schema, &relation.name);
        match relation.kind {
            RelationKind::Table | RelationKind::PartitionedTable => {}
            // MySQL refuses `ALTER TABLE` on a view outright, and there is no
            // `ALTER VIEW … COLUMN`: a view's columns are its query's.
            kind => {
                return Err(DbError::new(format!(
                    "{name} is a {kind:?}, and its columns come from its query rather than \
                     from a definition that can be altered"
                )));
            }
        }
        crate::column_change_text(
            &dbsql::MYSQL,
            "TABLE",
            &name,
            change,
            word,
            NullStyle::Suffix,
            AlterStyle::DefaultOnly(
                "MySQL changes a column's type or its nullability only by restating the whole \
                 declaration, which would drop the character set, the collation, the \
                 AUTO_INCREMENT and the comment this build never read. The default is the one \
                 property it can change on its own",
            ),
        )
    }

    /// All three are written above.
    fn changes_columns(&self) -> bool {
        true
    }

    /// The default, and nothing else.
    ///
    /// `MySQLTableColumnManager.addObjectModifyActions` writes
    /// `MODIFY COLUMN <whole declaration>`, and its `getNestedDeclaration`
    /// restates the AUTO_INCREMENT, the primary key and the comment because it
    /// has them to restate. This build does not, so the same statement written
    /// from what it knows would take those away — the hazard that already
    /// decided the rename above. `ALTER COLUMN … SET DEFAULT` touches the
    /// default alone, so that much is written and the rest is refused by name.
    fn alters_columns(&self) -> bool {
        true
    }

    /// `CREATE SCHEMA` and `DROP SCHEMA`, as `MySQLDatabaseManager` writes them.
    ///
    /// `SCHEMA` and not `DATABASE`, which upstream chose and MySQL treats as the
    /// same keyword — kept because the manager writing it is the one this is
    /// read from, and because a reader comparing the two files should find the
    /// same word in both.
    ///
    /// The name goes through the dialect's quoter rather than being wrapped in
    /// backticks by hand, which is a deliberate divergence: upstream writes
    /// `"CREATE SCHEMA \`" + name + "\`"`, and a name holding a backtick ends
    /// that identifier early and leaves the rest of it as syntax. `quote` doubles
    /// the closer, which is what MySQL reads as one.
    fn database_change(&self, change: DatabaseChange<'_>) -> DbResult<String> {
        let mut script = Script::new();
        script.statement(&match change {
            DatabaseChange::Create { name } => {
                format!("CREATE SCHEMA {}", dbsql::MYSQL.quote(name))
            }
            DatabaseChange::Drop { name } => format!("DROP SCHEMA {}", dbsql::MYSQL.quote(name)),
        });
        Ok(script.finish())
    }

    fn changes_databases(&self) -> bool {
        true
    }
}

/// The statement `SHOW CREATE {object}` answers with, taken from `column`.
///
/// By name rather than by position, as upstream reads it: `SHOW CREATE VIEW`
/// answers with four columns — the name, the statement, and the two character
/// sets the session had — where `SHOW CREATE TABLE` answers with two, and a
/// renderer that counted from the left would print a view's name as its DDL.
async fn shown(
    driver: &dyn Driver,
    relation: &RelationInfo,
    object: &str,
    column: &str,
) -> DbResult<String> {
    let name = qualified(&relation.schema, &relation.name);
    let sql = format!("SHOW CREATE {object} {name}");

    // One row is the whole result, so the batch size only decides how much of a
    // buffer is allocated to hold it.
    let mut stream = driver.query(&sql, 1).await?;
    let batch = stream.next_batch().await?.ok_or_else(|| {
        // Upstream returns the string "DDL is not available" here and shows it
        // in the tab. A statement that the server answered with no rows at all
        // means the object went away between the navigator listing it and this
        // call, and saying so beats a pane of text that reads like DDL.
        DbError::new(format!(
            "{name} is listed but the server has no {object} by that name"
        ))
    })?;
    let index = batch.schema().index_of(column).map_err(|_| {
        DbError::new(format!(
            "SHOW CREATE {object} answered without a {column:?} column"
        ))
    })?;
    let statements = batch.column(index);
    let statements = statements
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            DbError::new(format!(
                "{column:?} arrived as {} rather than as text",
                statements.data_type()
            ))
        })?;
    if statements.is_null(0) {
        return Err(DbError::new(format!("the server has no DDL for {name}")));
    }
    Ok(statements.value(0).to_string())
}

/// A view's statement with upstream's head in front of it.
///
/// `MySQLView.getAdditionalInfo` finds ` VIEW \`` and replaces everything before
/// it, which is where `DEFINER=\`root\`@\`%\`` and `SQL SECURITY DEFINER` go.
/// The algorithm survives because it changes how the view is planned; the
/// account that created it does not, because it says nothing about the view and
/// stops the DDL from running anywhere else.
///
/// A statement that has no ` VIEW \`` in it is passed through unchanged, which
/// is upstream's `divPos != -1` and is what happens for a name so plain that
/// MySQL wrote it without backticks.
fn replacing(shown: &str) -> String {
    let Some(at) = shown.find(" VIEW `") else {
        return shown.to_string();
    };
    match algorithm(&shown[..at]) {
        Some(algorithm) => format!("CREATE OR REPLACE ALGORITHM={algorithm}{}", &shown[at..]),
        None => format!("CREATE OR REPLACE{}", &shown[at..]),
    }
}

/// The `ALGORITHM=` of a view's head, where it has one.
///
/// Upstream matches `ALGORITHM\s*=\s*([A-Z_]+) ` and this reads the same thing
/// by hand, down to the trailing space: the name has to end at one, so a head
/// truncated mid-word is not read as an algorithm. A regular expression for one
/// fixed keyword would be a dependency this crate does not otherwise have.
fn algorithm(head: &str) -> Option<&str> {
    let after = head.split_once("ALGORITHM")?.1.trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let end = after.find(|c: char| !(c.is_ascii_uppercase() || c == '_'))?;
    after[end..].starts_with(' ').then(|| &after[..end])
}

/// `schema.name`, quoted the way MySQL reads it.
fn qualified(schema: &str, name: &str) -> String {
    format!(
        "{}.{}",
        dbsql::MYSQL.quote(schema),
        dbsql::MYSQL.quote(name)
    )
}

fn word(kind: ColumnKind) -> String {
    match kind {
        ColumnKind::Bool => "BOOLEAN".to_string(),
        ColumnKind::Int => "BIGINT".to_string(),
        ColumnKind::Float => "DOUBLE".to_string(),
        ColumnKind::Decimal(precision, scale) => format!("DECIMAL({precision}, {scale})"),
        ColumnKind::Text => "TEXT".to_string(),
        ColumnKind::Date => "DATE".to_string(),
        // `DATETIME` and not `TIMESTAMP`, which on MySQL holds 1970 to 2038 and
        // nothing outside it. A file of birthdays would import as a column of
        // errors, and the name that reads like the right one is the wrong one.
        ColumnKind::Timestamp => "DATETIME".to_string(),
    }
}
