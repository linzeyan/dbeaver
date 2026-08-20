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

/// One cell's value, as a predicate the browse's filter field can hold.
///
/// The front end asks rather than composing it, for the reason `statements`
/// exists: quoting is the database's own, and a value's spelling depends on the
/// type its column was declared with. It also has no way to be right about NULL,
/// which is what `clause` is about.
#[derive(Debug, Deserialize)]
pub struct CellFilter {
    pub schema: String,
    pub relation: String,
    pub column: String,
    pub op: FilterOp,
    /// The cell's value as the grid holds it, or `None` for a NULL cell.
    /// Ignored by the two operators that ask about NULL directly.
    pub value: Option<String>,
}

/// What a filter over one column can ask.
///
/// The first four are the questions whose answer is the same in every dialect
/// this build speaks, and they are the four the grid's cell menu offers: a menu
/// item has to be right without being read. The rest arrived with the filter
/// rows, where the column, its declared type and the operator sit on screen
/// together, so a pairing that makes no sense is visible before it runs.
///
/// That division is a fact about what a caller offers, not about what is
/// compiled here — every variant below becomes a predicate by the same route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Equals,
    NotEquals,
    IsNull,
    IsNotNull,
    LessThan,
    LessOrEqual,
    GreaterThan,
    GreaterOrEqual,
    /// `LIKE '%v%'`, with everything in `v` that `LIKE` reads as syntax written
    /// back as the character that was typed.
    Contains,
    StartsWith,
    EndsWith,
}

/// The predicate `filter` asks for, written in this database's own quoting.
///
/// Reads the relation's columns for the reason `statements` does: the grid holds
/// every value as text, and whether this one is written bare or quoted is a fact
/// about the column rather than about the characters.
pub async fn cell_filter(
    driver: &dyn Driver,
    dialect: &'static Dialect,
    filter: &CellFilter,
) -> DbResult<String> {
    let columns = driver.columns(&filter.schema, &filter.relation).await?;
    let column = columns
        .iter()
        .find(|column| column.name == filter.column)
        .ok_or_else(|| {
            DbError::new(format!(
                "{}.{} has no column {}",
                filter.schema, filter.relation, filter.column
            ))
        })?;
    clause(dialect, column, filter.op, filter.value.as_deref())
}

/// One predicate over one column.
///
/// Split from `cell_filter` because everything decided here is decided without a
/// database: how the operators are spelled, and what happens to NULL.
///
/// A missing value means two different things and this is where they part. For
/// `equals` it is a NULL cell asking to match itself, which is a question with an
/// answer. For every operator added since, it is a filter row nobody has finished
/// typing, and there is no answer to guess: `x < NULL` is valid SQL and is never
/// true, so guessing would turn a half-typed row into an empty grid — which
/// reads as a fact about the table rather than as a row still being written.
///
/// A NULL cell asked to match itself becomes `IS NULL`, and its negation `IS NOT
/// NULL`. `= NULL` is never true in SQL — not even of a NULL — so the literal
/// reading of "filter to this cell's value" over an empty cell is a filter that
/// matches no rows at all, which reads as a broken command rather than as a
/// lesson in three-valued logic.
fn clause(
    dialect: &Dialect,
    column: &ColumnInfo,
    op: FilterOp,
    value: Option<&str>,
) -> DbResult<String> {
    let name = dialect.quote(&column.name);
    Ok(match (op, value) {
        (FilterOp::IsNull, _) | (FilterOp::Equals, None) => format!("{name} IS NULL"),
        (FilterOp::IsNotNull, _) | (FilterOp::NotEquals, None) => format!("{name} IS NOT NULL"),
        (FilterOp::Equals, value) => format!("{name} = {}", literal(dialect, column, value)?),
        // `<>` rather than `!=`: every database here takes both, and this one is
        // the standard's.
        (FilterOp::NotEquals, value) => format!("{name} <> {}", literal(dialect, column, value)?),
        (
            FilterOp::LessThan
            | FilterOp::LessOrEqual
            | FilterOp::GreaterThan
            | FilterOp::GreaterOrEqual
            | FilterOp::Contains
            | FilterOp::StartsWith
            | FilterOp::EndsWith,
            None,
        ) => {
            return Err(DbError::new(format!(
                "the filter on {} has no value to compare against",
                column.name
            )));
        }
        (FilterOp::LessThan, value) => format!("{name} < {}", literal(dialect, column, value)?),
        (FilterOp::LessOrEqual, value) => {
            format!("{name} <= {}", literal(dialect, column, value)?)
        }
        (FilterOp::GreaterThan, value) => format!("{name} > {}", literal(dialect, column, value)?),
        (FilterOp::GreaterOrEqual, value) => {
            format!("{name} >= {}", literal(dialect, column, value)?)
        }
        // The pattern is built from the escaped value and then quoted, in that
        // order. Escaping after quoting would escape the quoting.
        (FilterOp::Contains, Some(text)) => {
            like(dialect, &name, &format!("%{}%", escape_like(text)))?
        }
        (FilterOp::StartsWith, Some(text)) => {
            like(dialect, &name, &format!("{}%", escape_like(text)))?
        }
        (FilterOp::EndsWith, Some(text)) => {
            like(dialect, &name, &format!("%{}", escape_like(text)))?
        }
    })
}

/// A `LIKE` predicate whose pattern means the characters that were typed.
///
/// The `ESCAPE` clause is read off the dialect rather than assumed. Without one,
/// a value holding a `%` — a discount column is full of them — silently matches
/// anything, and the failure is a grid of the wrong rows with nothing on screen
/// saying so. A dialect with no clause is one this build cannot spell a
/// wildcard-safe `LIKE` for, and the operator is refused by name instead of
/// guessed at; the caller offering it is what stops anybody reaching this.
fn like(dialect: &Dialect, name: &str, pattern: &str) -> DbResult<String> {
    let Some(escape) = dialect.like_escape else {
        return Err(DbError::new(format!(
            "{} takes no ESCAPE clause, so this build writes no LIKE filter for it",
            dialect.name
        )));
    };
    Ok(format!(
        "{name} LIKE {} ESCAPE {}",
        dialect.string_literal(pattern),
        dialect.string_literal(escape)
    ))
}

/// `value` with everything `LIKE` reads as syntax written back as itself.
///
/// The backslash goes first. Escaping the per cent first would leave the
/// backslash pass adding a second escape to the one just written, and the
/// pattern would hold two characters where the user typed one.
///
/// The result is still a bare string: `string_literal` quotes it afterwards, and
/// on the dialects where a backslash also escapes inside a string literal that
/// pass doubles these again — which is correct, and is why the two are separate
/// steps rather than one.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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
            values.push(literal(self.dialect, column, cell.value.as_deref())?);
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
            literal(self.dialect, column, cell.value.as_deref())?
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
                literal(self.dialect, column, Some(value))?
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
}

/// One value, written so this database reads it as the type its column is.
///
/// Quoted unless the column is a number and the text is one. Quoting is the
/// safe default and not a concession: a quoted literal has no type of its own,
/// so the server casts it to the column, and dates, uuids, json and enums all
/// arrive intact that way. A number is the exception because quoting one is not
/// always harmless — a strict database refuses to compare `'42'` with an integer
/// column, and this is the value most likely to be in a `WHERE` clause.
///
/// Text that claims to be a number and is not is refused rather than quoted. It
/// is the one case where guessing would turn a typing mistake into a statement
/// that runs.
///
/// Free of `Table` because a caller writing a predicate over a column has this
/// question and none of the others: a filter names a set of rows, so it needs
/// neither the key that names one nor the metadata read that resolves it.
fn literal(dialect: &Dialect, column: &ColumnInfo, value: Option<&str>) -> DbResult<String> {
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
    Ok(dialect.string_literal(value))
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
