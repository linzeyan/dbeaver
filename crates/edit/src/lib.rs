//! The statements that carry a grid's changes back to the database.
//!
//! Two decisions are made here and everything else follows from them.
//!
//! **Values reach the server as literals, not as bound parameters.** The
//! alternative is a `Driver` that takes parameters, which is a change to seven
//! implementations and a typed value crossing the C ABI for every cell. What it
//! would buy is exactness for the types where text is lossy, and safety from
//! quoting mistakes. The safety is bought here instead, by refusing anything
//! that is not the shape the column says it is, and the exactness is bought by
//! the second decision. What literals buy in exchange is the thing a person
//! editing a database actually asks for: a statement they can read before it
//! runs, copy into the editor, and keep.
//!
//! **A row is identified by its primary key and nothing else.** A result with no
//! unique key is not editable, which is upstream's answer too, and it is the
//! reason the first decision is affordable: a key that is an integer, a uuid or
//! a short string survives the round trip through text exactly, and one that is
//! a float is a schema nobody should be editing a cell of. Matching on every
//! column instead — the usual alternative — would put a `WHERE` clause full of
//! timestamps and floats between the user and their row, and the failure mode of
//! that is not an error message, it is updating a different row.
//!
//! Nothing here runs anything. The statements go back to the caller, which sends
//! them the way it sends any other statement — inside its transaction, through
//! its cancel button, with its error positions.

use dbconn::{ColumnInfo, DbError, DbResult, Driver};
use dbsql::Dialect;
use serde::Deserialize;

/// Everything a grid has pending for one relation.
///
/// One relation, because that is what a browse shows and what a key identifies.
/// A query pane's result can join five tables and there is no answer to which of
/// them a cell belongs to that is right often enough to write into a database.
#[derive(Debug, Deserialize)]
pub struct Edits {
    pub schema: String,
    pub relation: String,
    #[serde(default)]
    pub updates: Vec<Update>,
    #[serde(default)]
    pub inserts: Vec<Insert>,
    #[serde(default)]
    pub deletes: Vec<Delete>,
}

/// A row that was there, with the cells that changed.
#[derive(Debug, Deserialize)]
pub struct Update {
    pub key: Vec<Cell>,
    pub set: Vec<Cell>,
}

/// A row that was not there. Columns left out get whatever the table says they
/// default to, which is the difference between an insert and an update.
#[derive(Debug, Deserialize)]
pub struct Insert {
    pub set: Vec<Cell>,
}

#[derive(Debug, Deserialize)]
pub struct Delete {
    pub key: Vec<Cell>,
}

/// One column and what it now holds, as text.
///
/// `None` is SQL's NULL and not an empty string. A grid has to be able to say
/// both — an empty text column and an absent value are different rows — and a
/// single string cannot.
#[derive(Debug, Deserialize)]
pub struct Cell {
    pub column: String,
    pub value: Option<String>,
}

/// The statements `edits` would take, in the order they have to be sent.
///
/// Reads the relation's columns to find the key and the types, which is one
/// metadata call rather than a promise from the caller: the front end knows
/// which cells were typed into and has no business deciding which of them
/// identifies a row.
///
/// Updates go first, then inserts, then deletes. A row updated and then deleted
/// in one batch is a wasted statement rather than an error, and an insert that
/// reuses a key a delete is about to free is the one ordering that cannot work —
/// so the delete goes last, where it also cannot take a row an update still
/// needs.
pub async fn statements(
    driver: &dyn Driver,
    dialect: &'static Dialect,
    edits: &Edits,
) -> DbResult<Vec<String>> {
    let columns = driver.columns(&edits.schema, &edits.relation).await?;
    if columns.is_empty() {
        return Err(DbError::new(format!(
            "{}.{} has no columns to change",
            edits.schema, edits.relation
        )));
    }
    let table = Table {
        dialect,
        qualified: format!(
            "{}.{}",
            dialect.quote(&edits.schema),
            dialect.quote(&edits.relation)
        ),
        columns: &columns,
    };

    let mut out = Vec::new();
    for update in &edits.updates {
        out.push(table.update(update)?);
    }
    for insert in &edits.inserts {
        out.push(table.insert(insert)?);
    }
    for delete in &edits.deletes {
        out.push(table.delete(delete)?);
    }
    Ok(out)
}

/// The relation being written to, and the rules for writing to it.
struct Table<'a> {
    dialect: &'static Dialect,
    qualified: String,
    columns: &'a [ColumnInfo],
}

impl Table<'_> {
    fn update(&self, update: &Update) -> DbResult<String> {
        if update.set.is_empty() {
            return Err(DbError::new("an update with nothing to set"));
        }
        let assignments = update
            .set
            .iter()
            .map(|cell| self.assignment(cell))
            .collect::<DbResult<Vec<_>>>()?;
        Ok(format!(
            "UPDATE {} SET {} WHERE {}",
            self.qualified,
            assignments.join(", "),
            self.matching(&update.key)?
        ))
    }

    /// One new row.
    ///
    /// An insert carrying no cells is a row of the table's own defaults and not
    /// a mistake: a grid's new row starts with every column untouched, and every
    /// untouched column is left out of the statement so the schema decides it —
    /// so a row where the user touched nothing is a row where the schema decides
    /// everything. The dialect answers how to spell that, because the databases
    /// here do not agree, and one of them has no spelling for it. Refused by name
    /// there rather than written anyway: `INSERT INTO t` with nothing after it is
    /// not a statement, and the refusal has to say which table and which
    /// database, because that pair is the whole of the reason.
    fn insert(&self, insert: &Insert) -> DbResult<String> {
        if insert.set.is_empty() {
            let Some(defaults) = self.dialect.default_row else {
                return Err(DbError::new(format!(
                    "{} cannot be given a row of defaults: {} has no way to write one",
                    self.qualified, self.dialect.name
                )));
            };
            return Ok(format!("INSERT INTO {} {defaults}", self.qualified));
        }
        let mut names = Vec::with_capacity(insert.set.len());
        let mut values = Vec::with_capacity(insert.set.len());
        for cell in &insert.set {
            let column = self.column(&cell.column)?;
            names.push(self.dialect.quote(&column.name));
            values.push(self.literal(column, cell.value.as_deref())?);
        }
        Ok(format!(
            "INSERT INTO {} ({}) VALUES ({})",
            self.qualified,
            names.join(", "),
            values.join(", ")
        ))
    }

    fn delete(&self, delete: &Delete) -> DbResult<String> {
        Ok(format!(
            "DELETE FROM {} WHERE {}",
            self.qualified,
            self.matching(&delete.key)?
        ))
    }

    fn assignment(&self, cell: &Cell) -> DbResult<String> {
        let column = self.column(&cell.column)?;
        Ok(format!(
            "{} = {}",
            self.dialect.quote(&column.name),
            self.literal(column, cell.value.as_deref())?
        ))
    }

    /// The `WHERE` clause that names one row.
    ///
    /// Every primary-key column has to be here and nothing else may be. Too few
    /// and the statement changes a set of rows; a column that is not part of the
    /// key adds a condition that can be false for the row the user was looking
    /// at, so the edit silently does nothing.
    fn matching(&self, key: &[Cell]) -> DbResult<String> {
        let expected: Vec<&ColumnInfo> = self
            .columns
            .iter()
            .filter(|column| column.is_primary_key)
            .collect();
        if expected.is_empty() {
            return Err(DbError::new(format!(
                "{} has no primary key, so there is no way to name one row of it",
                self.qualified
            )));
        }
        let mut conditions = Vec::with_capacity(expected.len());
        for column in expected {
            let cell = key
                .iter()
                .find(|cell| cell.column == column.name)
                .ok_or_else(|| {
                    DbError::new(format!(
                        "the key column {} is missing from this row",
                        column.name
                    ))
                })?;
            // A key column that is null is either a row this result did not come
            // from or a key that is not one. `= NULL` is never true, so the
            // statement would run and change nothing.
            let value = cell.value.as_deref().ok_or_else(|| {
                DbError::new(format!("the key column {} has no value", column.name))
            })?;
            conditions.push(format!(
                "{} = {}",
                self.dialect.quote(&column.name),
                self.literal(column, Some(value))?
            ));
        }
        if key.len() > conditions.len() {
            return Err(DbError::new(
                "a row is named by its primary key and nothing else",
            ));
        }
        Ok(conditions.join(" AND "))
    }

    fn column(&self, name: &str) -> DbResult<&ColumnInfo> {
        self.columns
            .iter()
            .find(|column| column.name == name)
            .ok_or_else(|| DbError::new(format!("{} has no column {name}", self.qualified)))
    }

    /// One value, written so this database reads it as the type its column is.
    ///
    /// Quoted unless the column is a number and the text is one. Quoting is the
    /// safe default and not a concession: a quoted literal has no type of its
    /// own, so the server casts it to the column, and dates, uuids, json and
    /// enums all arrive intact that way. A number is the exception because
    /// quoting one is not always harmless — a strict database refuses to compare
    /// `'42'` with an integer column, and this is the value most likely to be in
    /// a `WHERE` clause.
    ///
    /// Text that claims to be a number and is not is refused rather than quoted.
    /// It is the one case where guessing would turn a typing mistake into a
    /// statement that runs.
    fn literal(&self, column: &ColumnInfo, value: Option<&str>) -> DbResult<String> {
        let Some(value) = value else {
            return Ok("NULL".to_string());
        };
        if numeric(&column.data_type) {
            return if value.trim().parse::<f64>().is_ok() {
                Ok(value.trim().to_string())
            } else {
                Err(DbError::new(format!(
                    "{} is a {} and {value:?} is not a number",
                    column.name, column.data_type
                )))
            };
        }
        Ok(self.quoted(value))
    }

    /// A string literal this database will read as the characters it was given.
    ///
    /// The quote is doubled everywhere, which is the SQL standard's own escape.
    /// The backslash is doubled only where the dialect says a backslash escapes
    /// — MySQL and ClickHouse — because on PostgreSQL, where it does not, a
    /// doubled backslash is two backslashes and the value comes back changed.
    fn quoted(&self, value: &str) -> String {
        let escaped = if self.dialect.backslash_escapes {
            value.replace('\\', "\\\\").replace('\'', "''")
        } else {
            value.replace('\'', "''")
        };
        format!("'{escaped}'")
    }
}

/// Whether a column's declared type holds numbers.
///
/// The name is taken up to its first `(` or space and then matched exactly,
/// which is what makes `numeric(18, 4)`, `tinyint(1)` and `bigint unsigned` the
/// types they say they are without `interval` becoming an integer for starting
/// with the same three letters.
///
/// The vocabulary is not one language: PostgreSQL says `int4` and `float8`,
/// MySQL `bigint`, SQL Server `decimal`, SQLite whatever the table was declared
/// with. The list is the union of what those produce, and a spelling missing
/// from it is quoted instead — the safe way round, because the worst a quoted
/// number meets is a database strict about casting, while an unquoted string is
/// a syntax error in the middle of somebody's data.
///
/// `money` and `bit` are deliberately absent although both hold something
/// numeric: PostgreSQL renders money as `$1,234.00` and a bit string as `101`,
/// and both of those are read correctly only in quotes.
fn numeric(data_type: &str) -> bool {
    const NUMERIC: &[&str] = &[
        "int",
        "int2",
        "int4",
        "int8",
        "integer",
        "smallint",
        "mediumint",
        "bigint",
        "tinyint",
        "serial",
        "smallserial",
        "bigserial",
        "float",
        "float4",
        "float8",
        "double",
        "real",
        "numeric",
        "decimal",
        "dec",
    ];
    let named = data_type
        .trim()
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    NUMERIC.contains(&named.as_str())
}
