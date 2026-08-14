//! Schema introspection for the navigator sidebar.
//!
//! Read through pragmas rather than queried out of a catalog. SQLite's
//! `sqlite_schema` holds only names and the DDL text they were created from, so
//! everything structured — a column's type, an index's keys, a foreign key's
//! sides — comes from a pragma instead. The table-valued form (`SELECT … FROM
//! pragma_table_info(…)`) is used throughout, because it takes a bound parameter
//! where the statement form would need the table name pasted into the SQL.
//!
//! Three things SQLite's catalog simply does not record, listed here rather than
//! discovered later by someone wondering where they went:
//!
//! - **Row counts.** There is no planner estimate unless `ANALYZE` has been run,
//!   and counting for a sidebar is not acceptable. Hence `Option`.
//! - **Foreign key names.** `pragma_foreign_key_list` returns the sides and not
//!   the name, even where one was written, so the name here is made up from the
//!   table and the key's position.
//! - **CHECK constraints.** They exist only inside the `CREATE TABLE` text.
//!   Extracting them means parsing SQL, which is `crates/sql`'s job in Phase 3
//!   and not something to half-do here.

use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;

use crate::SqliteError;

#[derive(Debug, Clone, Serialize)]
pub struct SchemaInfo {
    pub name: String,
}

/// What kind of relation a navigator entry is.
///
/// Shorter than PostgreSQL's list, and not a subset of it: SQLite has no
/// materialized views, foreign tables or partitioned tables, and it does have
/// virtual tables, which are a relation whose rows come from an extension rather
/// than from the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RelationKind {
    Table,
    View,
    Virtual,
    Unknown,
}

impl RelationKind {
    fn from_table_list(kind: &str) -> Self {
        match kind {
            "table" => Self::Table,
            "view" => Self::View,
            "virtual" => Self::Virtual,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationInfo {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    /// What `ANALYZE` last recorded, and `None` where it has never been run.
    ///
    /// Absent rather than zero, which PostgreSQL's driver should learn from: it
    /// clamps an unanalyzed relation's -1 up to 0, and a sidebar that says a
    /// table has no rows when it has not been asked is stating something false
    /// rather than declining to answer.
    pub estimated_rows: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    /// The type as declared. `ANY` where a column was declared without one,
    /// which SQLite permits and which is the word its own STRICT tables use for
    /// a column that holds any storage class.
    pub data_type: String,
    pub nullable: bool,
    pub position: i32,
    pub is_primary_key: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    /// Always `btree`. Carried anyway, so the shape does not change per driver
    /// for a field the sidebar prints either way.
    pub method: String,
    pub predicate: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationshipInfo {
    /// Made up from the table and the key's position: SQLite does not record the
    /// name a foreign key was declared with.
    pub name: String,
    pub local_columns: Vec<String>,
    pub other_schema: String,
    pub other_table: String,
    pub other_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstraintKind {
    Unique,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConstraintInfo {
    pub name: String,
    pub kind: ConstraintKind,
    pub definition: String,
}

/// A trigger, as much of one as SQLite records.
///
/// Nothing like PostgreSQL's. There the catalog holds the timing, the events and
/// the function in columns; here there is a name and the statement it was
/// created from, and picking `BEFORE` or `INSERT` out of that text is guessing at
/// something the user can read for themselves. Upstream's SQLite plugin reaches
/// the same conclusion and surfaces a name and the DDL.
#[derive(Debug, Clone, Serialize)]
pub struct TriggerInfo {
    pub name: String,
    pub definition: Option<String>,
}

/// Fails unless `schema` names a database this connection has open.
///
/// Checked up front so that every call answers the same way. Without it the
/// pragma-backed calls return an empty list for a schema that does not exist
/// while the `sqlite_schema`-backed ones fail with "no such table:
/// nowhere.sqlite_schema" — an internal detail, about a table the caller never
/// mentioned, in answer to a question about a schema.
///
/// An error rather than an empty answer, because a navigator asking about a
/// schema that is not open is working from a tree that has gone stale, and a
/// silent empty list is how that goes unnoticed.
pub(crate) fn require_schema(conn: &Connection, schema: &str) -> Result<(), SqliteError> {
    let known: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name = ?1)",
        [schema],
        |row| row.get(0),
    )?;
    if known {
        Ok(())
    } else {
        Err(SqliteError::NoSuchSchema(schema.to_string()))
    }
}

/// A schema name, ready to be pasted into SQL.
///
/// Pasted rather than bound, because a schema qualifies a table name and SQLite
/// has no parameter that can stand in that position. Doubling the quote is what
/// keeps `ATTACH '…' AS "od""d"` from becoming a second statement.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The databases attached to this connection.
///
/// In practice `main`, and that is worth saying plainly: `ATTACH` binds a second
/// database to one connection, and every call here opens a connection of its
/// own, so nothing a previous call attached is visible to the next. Making
/// `ATTACH` work means the source remembering the attachments and replaying them
/// on each connection, which is a feature nothing has asked for yet — the schema
/// argument is threaded through and honoured so that it is an addition rather
/// than a change when something does.
///
/// `temp` is left out for the same reason: it holds one connection's temporary
/// tables, so here it can never have anything under it.
pub(crate) fn schemas(conn: &Connection) -> Result<Vec<SchemaInfo>, SqliteError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_database_list WHERE name <> 'temp'")?;
    let rows = stmt.query_map([], |row| Ok(SchemaInfo { name: row.get(0)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn relations(conn: &Connection, schema: &str) -> Result<Vec<RelationInfo>, SqliteError> {
    // Shadow tables are excluded along with the `sqlite_` ones: they are the
    // storage a virtual table keeps for itself, listed beside the table they
    // belong to as though they were the user's.
    let mut stmt = conn.prepare(
        "SELECT name, type FROM pragma_table_list \
         WHERE schema = ?1 AND type IN ('table', 'view', 'virtual') \
           AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
         ORDER BY name",
    )?;
    let named = stmt.query_map([schema], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let estimates = row_estimates(conn, schema)?;
    named
        .map(|row| {
            let (name, kind) = row?;
            Ok(RelationInfo {
                schema: schema.to_string(),
                estimated_rows: estimates.get(&name).copied(),
                kind: RelationKind::from_table_list(&kind),
                name,
            })
        })
        .collect()
}

/// What `ANALYZE` recorded, by table name, or nothing if it has never run.
///
/// `sqlite_stat1.stat` is a space-separated list whose first entry is the row
/// count; the rest describe an index's selectivity and are not wanted here. The
/// table does not exist until `ANALYZE` creates it, which is why its absence is
/// an empty answer rather than an error.
fn row_estimates(conn: &Connection, schema: &str) -> Result<HashMap<String, i64>, SqliteError> {
    let qualified = quote_ident(schema);
    let exists: bool = conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {qualified}.sqlite_schema WHERE name = 'sqlite_stat1')"
        ),
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(HashMap::new());
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT tbl, CAST(substr(stat, 1, instr(stat || ' ', ' ') - 1) AS INTEGER) \
         FROM {qualified}.sqlite_stat1 WHERE idx IS NULL OR idx = tbl"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn columns(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<ColumnInfo>, SqliteError> {
    // `table_xinfo` rather than `table_info`, for generated columns: they are the
    // user's columns and `table_info` hides them. `hidden = 1` is left out — that
    // is a virtual table's internal column, which is the module's business.
    let mut stmt = conn.prepare(
        "SELECT name, type, \"notnull\", dflt_value, pk, cid \
         FROM pragma_table_xinfo(?1, ?2) WHERE hidden <> 1 ORDER BY cid",
    )?;
    let rows = stmt.query_map([relation, schema], |row| {
        let declared: String = row.get(1)?;
        let not_null: bool = row.get(2)?;
        let primary_key: i32 = row.get(4)?;
        let cid: i32 = row.get(5)?;
        Ok(ColumnInfo {
            name: row.get(0)?,
            data_type: if declared.is_empty() {
                "ANY".to_string()
            } else {
                declared
            },
            nullable: !not_null,
            // One-based, as PostgreSQL's `attnum` is. SQLite counts from zero,
            // and a front end showing both would show the same column as first
            // and as zeroth depending on which database it came from.
            position: cid + 1,
            is_primary_key: primary_key > 0,
            default_value: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The statement a view is defined by; `None` for a relation that has none.
///
/// The whole `CREATE VIEW … AS …`, not just the query, because that is what
/// SQLite stores — the text it was given, kept verbatim. PostgreSQL's driver
/// reports only the body, because `pg_get_viewdef` renders the body from the
/// parse tree and there is no original text to return.
pub(crate) fn definition(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Option<String>, SqliteError> {
    let qualified = quote_ident(schema);
    let mut stmt = conn.prepare(&format!(
        "SELECT sql FROM {qualified}.sqlite_schema WHERE type = 'view' AND name = ?1"
    ))?;
    let mut rows = stmt.query_map([relation], |row| row.get::<_, Option<String>>(0))?;
    match rows.next() {
        Some(row) => Ok(row?),
        None => Ok(None),
    }
}

pub(crate) fn indexes(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<IndexInfo>, SqliteError> {
    // A table whose primary key is `INTEGER PRIMARY KEY` has no index for it —
    // the key is the rowid — so this list can come back without a primary
    // entry for a table that plainly has one. `ColumnInfo::is_primary_key` is
    // where a front end should read the key from; this is where it reads what
    // the planner can use.
    let mut stmt =
        conn.prepare("SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1, ?2)")?;
    let listed: Vec<(String, bool, String, bool)> = stmt
        .query_map([relation, schema], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;

    let qualified = quote_ident(schema);
    listed
        .into_iter()
        .map(|(name, is_unique, origin, partial)| {
            let sql: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT sql FROM {qualified}.sqlite_schema \
                         WHERE type = 'index' AND name = ?1"
                    ),
                    [&name],
                    |row| row.get(0),
                )
                .unwrap_or(None);
            let (declared_keys, predicate) = match sql.as_deref() {
                Some(ddl) => index_ddl_parts(ddl),
                // An index SQLite created for a UNIQUE or PRIMARY KEY clause has
                // no statement of its own, and no predicate either.
                None => (Vec::new(), None),
            };
            Ok(IndexInfo {
                columns: index_keys(conn, schema, &name, &declared_keys)?,
                is_primary: origin == "pk",
                predicate: if partial { predicate } else { None },
                name,
                is_unique,
                method: "btree".to_string(),
            })
        })
        .collect()
}

/// One index's key expressions, in index order.
///
/// `pragma_index_info` names a key that is a column and answers NULL for one
/// that is an expression, so an index on `lower(email)` would otherwise come
/// back as a blank. The blanks are filled from the statement the index was
/// created from, by position — an index on `email` is not an index on
/// `lower(email)`, and printing one as the other misstates what the planner can
/// use.
fn index_keys(
    conn: &Connection,
    schema: &str,
    index: &str,
    declared: &[String],
) -> Result<Vec<String>, SqliteError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_index_info(?1, ?2) ORDER BY seqno")?;
    let named: Vec<Option<String>> = stmt
        .query_map([index, schema], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(named
        .into_iter()
        .enumerate()
        .map(|(position, name)| {
            name.or_else(|| declared.get(position).cloned())
                .unwrap_or_else(|| "?".to_string())
        })
        .collect())
}

pub(crate) fn foreign_keys(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, SqliteError> {
    let mut stmt = conn.prepare(
        "SELECT \"id\", \"table\", \"from\", \"to\", \"on_update\", \"on_delete\" \
         FROM pragma_foreign_key_list(?1, ?2) ORDER BY \"id\", \"seq\"",
    )?;
    let rows = stmt.query_map([relation, schema], |row| {
        Ok(ForeignKeyRow {
            id: row.get(0)?,
            declared_by: relation.to_string(),
            other_table: row.get(1)?,
            local_column: row.get(2)?,
            other_column: row.get(3)?,
            on_update: row.get(4)?,
            on_delete: row.get(5)?,
        })
    })?;
    group(conn, schema, relation, rows.collect::<Result<Vec<_>, _>>()?)
}

/// Foreign keys other relations declare against this one.
///
/// Every table's key list has to be read to find them: SQLite records a key on
/// the table that declares it and nowhere else, so there is no index to look up
/// the other direction in. Joining against the table-valued pragma does that in
/// one statement rather than one per table.
pub(crate) fn referenced_by(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, SqliteError> {
    let qualified = quote_ident(schema);
    let mut stmt = conn.prepare(&format!(
        "SELECT f.\"id\", m.name, f.\"to\", f.\"from\", f.\"on_update\", f.\"on_delete\" \
         FROM {qualified}.sqlite_schema m \
         JOIN pragma_foreign_key_list(m.name, ?1) f \
         WHERE m.type = 'table' AND f.\"table\" = ?2 COLLATE NOCASE \
         ORDER BY m.name, f.\"id\", f.\"seq\""
    ))?;
    // The sides are read swapped, so that every field is named for the relation
    // that was asked about rather than for the one that declared the key.
    let rows: Vec<ForeignKeyRow> = stmt
        .query_map([schema, relation], |row| {
            let declaring: String = row.get(1)?;
            Ok(ForeignKeyRow {
                id: row.get(0)?,
                declared_by: declaring.clone(),
                other_table: declaring,
                // NULL where the key names no columns on this side, meaning the
                // primary key. Resolved below, where the table it belongs to is
                // known.
                local_column: row.get(2)?,
                other_column: row.get(3)?,
                on_update: row.get(4)?,
                on_delete: row.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    group(conn, schema, relation, rows)
}

/// One column of one foreign key, as the pragma reports it.
struct ForeignKeyRow {
    id: i64,
    /// The table the key was written on, which is not always the one that was
    /// asked about: looked at from the referenced side, the key still belongs to
    /// whoever declared it, and the made-up name has to say so.
    declared_by: String,
    other_table: String,
    /// `None` where the pragma left it out, which happens on the referenced side
    /// of a key written without one — it means the primary key.
    local_column: Option<String>,
    other_column: Option<String>,
    on_update: String,
    on_delete: String,
}

/// Collects a key's columns back together, in the order they were declared.
///
/// A composite key arrives one column per row and the two sides have to line up:
/// the third local column references the third foreign one. Ordering by `seq`,
/// which the queries above do, is what keeps that true.
fn group(
    conn: &Connection,
    schema: &str,
    relation: &str,
    rows: Vec<ForeignKeyRow>,
) -> Result<Vec<RelationshipInfo>, SqliteError> {
    let mut keys: Vec<RelationshipInfo> = Vec::new();
    let mut current: Option<(i64, String)> = None;

    for row in rows {
        let belongs_to_current = current
            .as_ref()
            .is_some_and(|(id, table)| *id == row.id && *table == row.declared_by);
        if !belongs_to_current {
            current = Some((row.id, row.declared_by.clone()));
            keys.push(RelationshipInfo {
                // SQLite does not keep the declared name, so this is built from
                // what it does keep. The position disambiguates two keys between
                // the same pair of tables.
                name: format!("fk_{}_{}", row.declared_by, row.id),
                local_columns: Vec::new(),
                other_schema: schema.to_string(),
                other_table: row.other_table,
                other_columns: Vec::new(),
                on_update: row.on_update,
                on_delete: row.on_delete,
            });
        }
        let key = keys.last_mut().expect("a key was pushed above");
        if let Some(column) = row.local_column {
            key.local_columns.push(column);
        }
        if let Some(column) = row.other_column {
            key.other_columns.push(column);
        }
    }

    // A key written without naming the far side references that table's primary
    // key. The pragma leaves the column out rather than filling it in, so it is
    // filled in here — a key rendered with one side blank reads as though the
    // database were missing something.
    for key in &mut keys {
        if key.other_columns.is_empty() {
            key.other_columns = primary_key_columns(conn, schema, &key.other_table)?;
        }
        if key.local_columns.is_empty() {
            key.local_columns = primary_key_columns(conn, schema, relation)?;
        }
    }
    Ok(keys)
}

fn primary_key_columns(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<String>, SqliteError> {
    let mut stmt =
        conn.prepare("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk")?;
    let rows = stmt.query_map([relation, schema], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// UNIQUE constraints, which SQLite records as indexes it created itself.
///
/// CHECK constraints are not here, and their absence is the point of the module
/// comment: they live only in the `CREATE TABLE` text, and reading them out of
/// it is parsing SQL.
pub(crate) fn constraints(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<ConstraintInfo>, SqliteError> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_index_list(?1, ?2) WHERE origin = 'u' ORDER BY name")?;
    let names: Vec<String> = stmt
        .query_map([relation, schema], |row| row.get(0))?
        .collect::<Result<_, _>>()?;

    names
        .into_iter()
        .map(|name| {
            let columns = index_keys(conn, schema, &name, &[])?;
            Ok(ConstraintInfo {
                definition: format!("UNIQUE ({})", columns.join(", ")),
                kind: ConstraintKind::Unique,
                name,
            })
        })
        .collect()
}

pub(crate) fn triggers(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<TriggerInfo>, SqliteError> {
    let qualified = quote_ident(schema);
    let mut stmt = conn.prepare(&format!(
        "SELECT name, sql FROM {qualified}.sqlite_schema \
         WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name"
    ))?;
    let rows = stmt.query_map([relation], |row| {
        Ok(TriggerInfo {
            name: row.get(0)?,
            definition: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The key expressions and the predicate of a `CREATE INDEX` statement.
///
/// A scan rather than a parse, and it only has to handle one statement shape:
/// `CREATE [UNIQUE] INDEX name ON table ( keys ) [WHERE predicate]`. What makes
/// it worth doing at all is that `pragma_index_info` cannot name an expression
/// key and no pragma reports a partial index's predicate — both would otherwise
/// be blanks in the sidebar for indexes that plainly have them.
///
/// Quoted text is stepped over rather than looked inside, because a table called
/// `"a(b)"` would otherwise be read as the start of the key list.
fn index_ddl_parts(sql: &str) -> (Vec<String>, Option<String>) {
    let mut chars = sql.char_indices().peekable();
    let mut depth = 0usize;
    let mut start = None;

    while let Some((idx, c)) = chars.next() {
        match c {
            '\'' | '"' | '`' => skip_quoted(&mut chars, c),
            '[' => skip_quoted(&mut chars, ']'),
            '(' => {
                if depth == 0 {
                    start = Some(idx + 1);
                }
                depth += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let keys = split_top_level(&sql[start.unwrap_or(idx)..idx]);
                    return (keys, predicate_after(&sql[idx + 1..]));
                }
            }
            _ => {}
        }
    }
    (Vec::new(), None)
}

/// Consumes up to and including the closing `end` of a quoted identifier or
/// string. A doubled quote inside one is a literal, and stepping over the pair
/// is what keeps it from being read as the end.
fn skip_quoted(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, end: char) {
    while let Some((_, c)) = chars.next() {
        if c == end {
            if chars.peek().map(|(_, next)| *next) == Some(end) {
                chars.next();
                continue;
            }
            return;
        }
    }
}

/// Splits a key list on the commas that separate keys, leaving the ones inside a
/// function call alone.
fn split_top_level(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut chars = list.char_indices().peekable();

    while let Some((idx, c)) = chars.next() {
        match c {
            '\'' | '"' | '`' => skip_quoted(&mut chars, c),
            '[' => skip_quoted(&mut chars, ']'),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(list[start..idx].trim().to_string());
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    let last = list[start..].trim();
    if !last.is_empty() {
        out.push(last.to_string());
    }
    out
}

/// What follows a partial index's `WHERE`, if there is one.
fn predicate_after(tail: &str) -> Option<String> {
    let trimmed = tail.trim_start();
    let (keyword, rest) = trimmed.split_at(trimmed.len().min(5));
    if !keyword.eq_ignore_ascii_case("WHERE") {
        return None;
    }
    let predicate = rest.trim().trim_end_matches(';').trim();
    (!predicate.is_empty()).then(|| predicate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_schema_name_survives_being_pasted_into_sql() {
        assert_eq!(quote_ident("main"), "\"main\"");
        // A name holding a quote is how a pasted identifier becomes a second
        // statement. ATTACH takes whatever name it is given.
        assert_eq!(quote_ident("o\"d"), "\"o\"\"d\"");
    }

    #[test]
    fn index_keys_come_out_of_the_statement_that_declared_them() {
        let (keys, predicate) = index_ddl_parts("CREATE INDEX i ON t (a, b)");
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(predicate, None);
    }

    #[test]
    fn an_expression_key_keeps_the_commas_inside_it() {
        // The reason this is not a `split(',')`: the comma in `substr(a, 1, 2)`
        // separates arguments, not keys.
        let (keys, _) = index_ddl_parts("CREATE INDEX i ON t (lower(email), substr(a, 1, 2))");
        assert_eq!(keys, ["lower(email)", "substr(a, 1, 2)"]);
    }

    #[test]
    fn a_parenthesis_inside_a_quoted_name_is_not_the_key_list() {
        let (keys, _) = index_ddl_parts("CREATE INDEX i ON \"a(b)\" (x)");
        assert_eq!(keys, ["x"]);
    }

    #[test]
    fn a_partial_index_reports_the_predicate_no_pragma_will() {
        let (keys, predicate) =
            index_ddl_parts("CREATE UNIQUE INDEX i ON t (a) WHERE deleted_at IS NULL;");
        assert_eq!(keys, ["a"]);
        assert_eq!(predicate.as_deref(), Some("deleted_at IS NULL"));
    }

    #[test]
    fn a_where_that_is_not_there_is_not_invented() {
        let (_, predicate) = index_ddl_parts("CREATE INDEX i ON t (a)");
        assert_eq!(predicate, None);
        // A key named `whereabouts` starts with the same five letters.
        let (_, predicate) = index_ddl_parts("CREATE INDEX i ON t (a) -- whereabouts");
        assert_eq!(predicate, None);
    }
}
