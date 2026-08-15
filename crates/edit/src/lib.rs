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
//! **A row is identified by a declared key and nothing else.** A result with no
//! unique key is not editable, which is upstream's answer too, and it is the
//! reason the first decision is affordable: a key that is an integer, a uuid or
//! a short string survives the round trip through text exactly, and one that is
//! a float is a schema nobody should be editing a cell of. Matching on every
//! column instead — the usual alternative — would put a `WHERE` clause full of
//! timestamps and floats between the user and their row, and the failure mode of
//! that is not an error message, it is updating a different row.
//!
//! The primary key is the first answer and a `UNIQUE` constraint the second,
//! which is upstream's order too. What a unique constraint has to prove before
//! it is used is in [`identity`]: SQL's `NULL != NULL` means a key over a column
//! that can be null names no row at all, so such a key is refused by name rather
//! than tried.
//!
//! Nothing here runs anything. The statements go back to the caller, which sends
//! them the way it sends any other statement — inside its transaction, through
//! its cancel button, with its error positions.

use dbconn::{ColumnInfo, DbError, DbResult, Driver, UniqueKeyInfo};
use dbsql::Dialect;
use serde::{Deserialize, Serialize};

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
/// Reads the relation's own metadata to find the key and the types rather than
/// taking a promise from the caller: the front end knows which cells were typed
/// into and has no business deciding which of them identifies a row.
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
    let qualified = format!(
        "{}.{}",
        dialect.quote(&edits.schema),
        dialect.quote(&edits.relation)
    );
    let key = resolve(driver, &edits.schema, &edits.relation, &qualified, &columns).await?;
    let table = Table {
        dialect,
        qualified,
        columns: &columns,
        key,
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

/// What names one row of a relation, for a caller that has to decide whether to
/// offer editing at all.
///
/// The same answer `statements` builds its `WHERE` clauses from, exported
/// because a front end needs it before anything has been typed: it has to know
/// which of the grid's columns to send back as the key, and it has to say why a
/// table is read-only rather than leaving a control mysteriously disabled. A
/// front end that worked the rule out for itself would be a second copy of it,
/// and the day either was corrected they would disagree.
#[derive(Debug, Clone, Serialize)]
pub struct RowIdentity {
    /// The columns whose values name one row, in key order. Empty when nothing
    /// does, which is the same question as whether the relation can be edited.
    pub columns: Vec<String>,
    /// Why there is nothing, in a sentence that names the table and the
    /// constraint it had to turn down. `None` when `columns` is not empty.
    pub obstacle: Option<String>,
}

/// What names one row of `relation`.
///
/// The primary key first, and a `UNIQUE` constraint only where there is none —
/// which is the order the decision was taken in, and also the only order that
/// keeps a schema's own answer ahead of this crate's choice among several.
///
/// A unique constraint has to prove two things before it is used:
///
/// **None of its columns may be nullable.** `NULL != NULL`, so a `WHERE` over a
/// nullable key column matches nothing where the row holds NULL, and where two
/// rows hold NULL the constraint permitted both — one key, several rows. Either
/// way it is not an identity, and the failure shows up as an edit that quietly
/// did nothing rather than as an error.
///
/// **Its columns have to be columns this relation has.** A driver that names one
/// the column list does not is a driver whose two answers disagree, and the key
/// it named cannot be looked up to find out whether it is nullable.
///
/// A constraint that fails either is refused by name, and the sentence says
/// which one and why: the pane showing it is the only place a person finds out
/// that the table with the obvious unique column is not editable, and "this
/// table cannot be edited" alone gives them nothing to change.
pub async fn identity(driver: &dyn Driver, schema: &str, relation: &str) -> DbResult<RowIdentity> {
    let columns = driver.columns(schema, relation).await?;
    if columns.is_empty() {
        return Ok(RowIdentity {
            columns: Vec::new(),
            obstacle: Some(format!("{schema}.{relation} has no columns")),
        });
    }
    // Unquoted, because this name is only ever read: it goes into a sentence for
    // somebody to act on, not into a statement. The schema is skipped where the
    // database has no schema layer to report — `.orders` is not a relation
    // anywhere, and this is the one string a user sees.
    let qualified = if schema.is_empty() {
        relation.to_string()
    } else {
        format!("{schema}.{relation}")
    };
    Ok(
        match resolve(driver, schema, relation, &qualified, &columns).await? {
            Ok(key) => RowIdentity {
                columns: key.columns.iter().map(|c| c.name.clone()).collect(),
                obstacle: None,
            },
            Err(obstacle) => RowIdentity {
                columns: Vec::new(),
                obstacle: Some(obstacle),
            },
        },
    )
}

/// The key `qualified`'s rows are named by, or the sentence saying there is
/// none.
///
/// Two nested results, and they are different failures. The outer one is the
/// catalog not answering, which is nobody's decision; the inner one is the
/// catalog answering that this table has nothing to name a row by, which is an
/// answer and is carried as text because that text is what somebody reads.
async fn resolve<'a>(
    driver: &dyn Driver,
    schema: &str,
    relation: &str,
    qualified: &str,
    columns: &'a [ColumnInfo],
) -> DbResult<Result<Key<'a>, String>> {
    let primary: Vec<&ColumnInfo> = columns.iter().filter(|c| c.is_primary_key).collect();
    if !primary.is_empty() {
        // The catalog's own answer, so nothing is chosen here and the extra
        // metadata call below is not made at all.
        return Ok(Ok(Key {
            columns: primary,
            source: "its primary key".to_string(),
        }));
    }

    let declared = driver.unique_keys(schema, relation).await?;
    let mut usable: Vec<(&UniqueKeyInfo, Vec<&ColumnInfo>)> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for key in &declared {
        match usable_columns(key, columns) {
            Ok(resolved) => usable.push((key, resolved)),
            Err(why) => refused.push(why),
        }
    }

    // Fewest columns first, then the constraint's own name. The second half is
    // the one that is easy to leave out and the one that matters most: a catalog
    // may return two constraints of the same width in whatever order it happened
    // to produce them, and an identity that depends on that is an identity
    // nobody can reason about — the same edit against the same schema writing a
    // different `WHERE` clause on Tuesday.
    //
    // Fewest columns first because every key column becomes a condition carrying
    // a value that went to the server as text: the narrower key has fewer
    // chances to be the one holding a timestamp or a float, and the shorter
    // statement is the one somebody can read before running it. The name breaks
    // the remaining ties because it is the only other thing the catalog reports
    // that is the same on two runs against one schema.
    usable.sort_by(|(a, columns_a), (b, columns_b)| {
        columns_a
            .len()
            .cmp(&columns_b.len())
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(match usable.into_iter().next() {
        Some((key, columns)) => Ok(Key {
            columns,
            source: format!("the unique key {}", key.name),
        }),
        None if refused.is_empty() => Err(format!(
            "{qualified} has no primary key or unique key, so there is no way to name one row of it"
        )),
        None => Err(format!(
            "{qualified} has no primary key, and {}, so there is no way to name one row of it",
            refused.join("; ")
        )),
    })
}

/// One unique constraint's columns, or why it cannot name a row.
fn usable_columns<'a>(
    key: &UniqueKeyInfo,
    columns: &'a [ColumnInfo],
) -> Result<Vec<&'a ColumnInfo>, String> {
    let mut resolved = Vec::with_capacity(key.columns.len());
    for name in &key.columns {
        let Some(column) = columns.iter().find(|c| &c.name == name) else {
            return Err(format!(
                "the unique key {} is over {name}, which this table has no column of",
                key.name
            ));
        };
        if column.nullable {
            return Err(format!(
                "the unique key {} is over {name}, which can be null",
                key.name
            ));
        }
        resolved.push(column);
    }
    if resolved.is_empty() {
        return Err(format!("the unique key {} is over no columns", key.name));
    }
    Ok(resolved)
}

/// The relation being written to, and the rules for writing to it.
struct Table<'a> {
    dialect: &'static Dialect,
    qualified: String,
    columns: &'a [ColumnInfo],
    /// What names one row here, or the sentence saying why nothing does.
    ///
    /// Resolved once for the whole batch and not once per statement: it is a
    /// fact about the relation, and asking the catalog again for every row would
    /// also allow two statements in one Save to disagree about what a row is.
    key: Result<Key<'a>, String>,
}

/// The columns that name one row, and what said so.
struct Key<'a> {
    columns: Vec<&'a ColumnInfo>,
    /// How to refer to it in a message — "its primary key", "the unique key
    /// uq_orders_email". A refusal that cannot name what it refused leaves the
    /// reader to guess which of a table's constraints was meant.
    source: String,
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

    fn insert(&self, insert: &Insert) -> DbResult<String> {
        if insert.set.is_empty() {
            return Err(DbError::new("an insert with no values"));
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
    /// Every column of the key has to be here and nothing else may be. Too few
    /// and the statement changes a set of rows; a column that is not part of the
    /// key adds a condition that can be false for the row the user was looking
    /// at, so the edit silently does nothing.
    fn matching(&self, key: &[Cell]) -> DbResult<String> {
        let identity = match &self.key {
            Ok(identity) => identity,
            Err(why) => return Err(DbError::new(why.clone())),
        };
        let mut conditions = Vec::with_capacity(identity.columns.len());
        for column in &identity.columns {
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
            return Err(DbError::new(format!(
                "a row is named by {} and nothing else",
                identity.source
            )));
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
