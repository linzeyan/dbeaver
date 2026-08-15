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

use crate::Renderer;
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
