//! The statements that would recreate what the navigator is showing.
//!
//! Upstream is not an influence here, it is the specification: the phase-4 exit
//! criterion is that this output matches DBeaver's for the same object, so every
//! rule below was read out of the Java rather than chosen. Where this
//! deliberately differs, the difference is recorded on the line that differs,
//! with what upstream emits and why this does not.
//!
//! Upstream assembles a table's DDL in three layers and so does this crate,
//! because only the third of them is per-database:
//!
//! - `model/impl/sql/edit/struct/SQLTableManager.getTableDDL` decides what goes
//!   into the script and in which order — drop header, `CREATE TABLE`, then
//!   whatever could not be written inside the parentheses.
//! - `model/sql/SQLUtils.generateScript` joins those into text.
//! - `ext.postgresql`'s managers supply the text of each column, constraint and
//!   index.
//!
//! The first two live in `org.jkiss.dbeaver.model` and are shared by every
//! database upstream supports, which is why [`Script`] lives here. The third is
//! per-database and lives behind [`Renderer`] — one implementation today, and
//! the seam is visible now because the next five arrive against it rather than
//! against a function that has learned to branch.
//!
//! What this does *not* do is read the database twice. Everything rendered comes
//! from the `Driver` metadata calls the structure pane already makes, which is
//! also the limit on what can be rendered: a fact upstream reads from a catalog
//! column that no metadata type carries is a fact this cannot state, and the
//! honest answer to that is a refusal rather than a guess.

mod postgres;

use async_trait::async_trait;
use dbconn::{DbError, DbResult, Driver, RelationInfo};
use dbsql::Dialect;

/// The DDL that would recreate `relation`, in the SQL `dialect` writes.
///
/// No `schema` parameter, although upstream's equivalent call sites pass one:
/// `RelationInfo` already carries the schema it was listed under, and a separate
/// argument is one more thing that can disagree with it. The relation is
/// identified by exactly what the navigator handed the caller.
pub async fn definition(
    driver: &dyn Driver,
    dialect: &'static Dialect,
    relation: &RelationInfo,
) -> DbResult<String> {
    match for_dialect(dialect) {
        Some(renderer) => renderer.definition(driver, relation).await,
        None => Err(DbError::new(format!(
            "DDL for {} has not been written yet",
            dialect.name
        ))),
    }
}

/// The half of DDL generation that is genuinely per-database.
///
/// One method, because that is how much the databases share. Upstream's own
/// split says the same thing: MySQL asks the server for `SHOW CREATE TABLE`,
/// SQLite keeps the statement it was created from, PostgreSQL builds one out of
/// the catalog — the *whole* of producing the text differs, and only the script
/// it goes into does not.
#[async_trait]
pub trait Renderer: Send + Sync {
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String>;
}

/// The renderer written for `dialect`, and `None` where none is yet.
///
/// A lookup, deliberately not a fallback: guessing that an unknown database
/// writes PostgreSQL's DDL would produce a statement that looks right and does
/// not run, which is worse than saying nothing. `dbsql::for_scheme` can afford
/// the opposite default because a wrong dialect there costs syntax colour.
pub fn for_dialect(dialect: &'static Dialect) -> Option<&'static dyn Renderer> {
    RENDERERS
        .iter()
        .find(|(known, _)| known.name == dialect.name)
        .map(|(_, renderer)| *renderer)
}

/// Every database whose DDL this build can write, in the order they arrived.
const RENDERERS: &[(&Dialect, &dyn Renderer)] = &[(&dbsql::POSTGRES, &postgres::POSTGRES)];

/// A script under construction, joined the way upstream joins one.
///
/// `SQLUtils.generateScript` has two rules and they are not symmetric. A
/// statement is followed by `;` — unless it brought its own — and one newline. A
/// comment gets a blank line before it, unless one is already there, and a blank
/// line after. That asymmetry is what puts the section headings in a table's DDL
/// on their own, and reproducing it by hand at each call site is how the third
/// heading ends up spaced differently from the first two.
pub(crate) struct Script(String);

impl Script {
    pub(crate) fn new() -> Self {
        Self(String::new())
    }

    pub(crate) fn statement(&mut self, sql: &str) {
        self.0.push_str(sql);
        if !sql.trim_end().ends_with(';') {
            self.0.push(';');
        }
        self.0.push('\n');
    }

    pub(crate) fn comment(&mut self, text: &str) {
        // Upstream counts the trailing newlines and adds one if there are fewer
        // than two; since everything written here ends in a newline already,
        // that reduces to "is there a blank line". An empty script gets nothing,
        // so a DDL that opens with a comment does not open with a blank line.
        if !self.0.is_empty() && !self.0.ends_with("\n\n") {
            self.0.push('\n');
        }
        self.0.push_str("-- ");
        self.0.push_str(text);
        self.0.push_str("\n\n");
    }

    /// The finished script, without the newline the last statement left.
    ///
    /// Upstream keeps it and the editor that shows the text trims it
    /// (`SQLSourceViewer.getSourceText`). Trimming here instead means a caller
    /// that writes this to a file, a clipboard or a test assertion gets the same
    /// string as one that shows it, rather than each of them deciding.
    pub(crate) fn finish(self) -> String {
        self.0.trim_end().to_string()
    }
}
