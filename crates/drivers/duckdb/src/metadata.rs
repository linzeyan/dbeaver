//! Schema introspection for the navigator sidebar.
//!
//! Read from the `duckdb_*` table functions rather than from
//! `information_schema`, which DuckDB also has. The deciding case is foreign
//! keys and it is decisive: `duckdb_constraints()` answers "which table does
//! this key reference, and with which columns on each side" in one row, with
//! both sides already in declaration order, where information_schema needs
//! `referential_constraints` joined through `table_constraints` and
//! `key_column_usage` to reach the same fact. Everything else agrees:
//! `duckdb_tables()` has the row estimate and `information_schema.tables` does
//! not, `duckdb_views()` has the statement a view was created from and
//! information_schema has nothing like it.
//!
//! **The parentheses are not optional.** Most `duckdb_x` functions have a
//! same-named view in `main` that hides the rows with `internal` set, so
//! `FROM duckdb_views()` and `FROM duckdb_views` are different queries. The
//! function form is used throughout and the filtering is written out, so it is
//! visible rather than inherited.
//!
//! ## Two levels flattened onto one
//!
//! DuckDB names a relation in three parts — database, schema, table — and the
//! shared `SchemaInfo` has room for one. So a schema here is called
//! `database.schema`: `warehouse.main`, `warehouse.app`. Three reasons, in the
//! order they mattered.
//!
//! `ATTACH` is ordinary DuckDB usage, and after one there are two schemas called
//! `main` with different contents in them. A sidebar showing two identical nodes
//! is worse than one showing a longer name.
//!
//! The qualified name is real SQL. `SELECT * FROM warehouse.main.orders` is what
//! DuckDB itself accepts, so a front end that pastes the name it was given into
//! a statement gets a statement that runs — which a bare `main` would not, once
//! a second database is attached.
//!
//! And it needs no parsing to undo. Every query below filters on
//! `database_name || '.' || schema_name = ?` rather than splitting the string,
//! so a database called `sales.2024` or a schema called `"a.b"` resolves exactly
//! instead of resolving to whichever half a `split_once` guessed at.
//!
//! What it costs: the common case, one database with one schema in it, reads
//! `warehouse.main` in the sidebar where every other driver reads `main`. That
//! is the visible price of the trait having one level where DuckDB has two.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo,
};
use duckdb::Connection;
use duckdb::types::Value;

use crate::DuckError;

/// Every schema a connection can reach, except the ones nothing can be under.
///
/// Filtered by database rather than by the `internal` flag, which is the trap
/// here. `duckdb_schemas()` reports `internal = true` for the `main` schema of
/// the user's own database — the schema everything they created is in — so the
/// obvious `WHERE NOT internal` returns a navigator with the user's tables
/// missing and their extra schemas present.
///
/// `system` holds `information_schema`, `pg_catalog` and DuckDB's own `main`.
/// `temp` holds one connection's temporary objects, and every call here runs on
/// a connection of its own, so it is a node that can never have anything under
/// it.
pub(crate) fn schemas(conn: &Connection) -> Result<Vec<SchemaInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT database_name || '.' || schema_name \
         FROM duckdb_schemas() \
         WHERE database_name NOT IN ('system', 'temp') \
         ORDER BY database_name, schema_name",
    )?;
    let rows = stmt.query_map([], |row| Ok(SchemaInfo { name: row.get(0)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Tables and views in one schema.
///
/// Two functions rather than `information_schema.tables`, which merges them and
/// loses `estimated_size`. There is no third kind to look for: DuckDB has no
/// materialized views, no foreign tables and no partitioned tables, and its
/// table functions are functions rather than relations in the catalog, so
/// SQLite's `Virtual` has nothing to describe here either.
pub(crate) fn relations(conn: &Connection, schema: &str) -> Result<Vec<RelationInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT table_name AS name, 'table' AS kind, estimated_size \
         FROM duckdb_tables() \
         WHERE database_name || '.' || schema_name = ? AND NOT internal \
         UNION ALL \
         SELECT view_name, 'view', CAST(NULL AS BIGINT) \
         FROM duckdb_views() \
         WHERE database_name || '.' || schema_name = ? AND NOT internal \
         ORDER BY name",
    )?;
    let rows = stmt.query_map([schema, schema], |row| {
        let kind: String = row.get(1)?;
        Ok(RelationInfo {
            schema: schema.to_string(),
            name: row.get(0)?,
            kind: if kind == "view" {
                RelationKind::View
            } else {
                RelationKind::Table
            },
            // DuckDB's own word for it is "estimated", and it is: a table that
            // has just been written to reports what the catalog last recorded.
            // A view has no equivalent, so it declines to answer rather than
            // reporting zero — which would state something false.
            estimated_rows: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Column definitions for one relation.
///
/// `data_type` is DuckDB's own rendering — `DECIMAL(18,4)`, `INTEGER[]`,
/// `STRUCT(qty INTEGER, unit VARCHAR)`, `ENUM('draft', 'live')` — and it is
/// load-bearing twice. It is what the structure pane shows, and it is the only
/// place several DuckDB types survive at all: `HUGEINT` and `UHUGEINT` and
/// `DECIMAL(38,0)` are one Arrow type, so are `UUID` and `VARCHAR` and `JSON`,
/// and so are `TIME` and `TIMETZ`. Letting the database produce Arrow makes the
/// wire form a rendering, and this is where the declaration is kept.
pub(crate) fn columns(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<ColumnInfo>, DuckError> {
    // `duckdb_columns()` has no primary-key flag; the key is in
    // `duckdb_constraints()` and nowhere else. A table has at most one PRIMARY
    // KEY constraint, so the join cannot fan the column list out.
    let mut stmt = conn.prepare(
        "SELECT c.column_name, c.data_type, c.is_nullable, c.column_index, c.column_default, \
                coalesce(list_contains(pk.constraint_column_names, c.column_name), false) \
         FROM duckdb_columns() c \
         LEFT JOIN ( \
             SELECT database_name, schema_name, table_name, constraint_column_names \
             FROM duckdb_constraints() WHERE constraint_type = 'PRIMARY KEY' \
         ) pk \
           ON pk.database_name = c.database_name \
          AND pk.schema_name = c.schema_name \
          AND pk.table_name = c.table_name \
         WHERE c.database_name || '.' || c.schema_name = ? AND c.table_name = ? \
         ORDER BY c.column_index",
    )?;
    let rows = stmt.query_map([schema, relation], |row| {
        Ok(ColumnInfo {
            name: row.get(0)?,
            data_type: row.get(1)?,
            nullable: row.get(2)?,
            // Documented as one-based and it is, so unlike SQLite's `cid` there
            // is nothing to shift.
            position: row.get(3)?,
            default_value: row.get(4)?,
            is_primary_key: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The statement a view is defined by; `None` for anything else.
///
/// The whole `CREATE VIEW … AS …` rather than the body, because that is what
/// DuckDB stores. `crates/drivers/sqlite/src/metadata.rs` says the same thing
/// about SQLite, and PostgreSQL differs because `pg_get_viewdef` renders the
/// body back from a parse tree and has no original text to return. The two
/// embedded drivers agreeing with each other and differing from the server one
/// is worth the trait knowing.
pub(crate) fn definition(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Option<String>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM duckdb_views() \
         WHERE database_name || '.' || schema_name = ? AND view_name = ?",
    )?;
    let mut rows = stmt.query_map([schema, relation], |row| row.get::<_, Option<String>>(0))?;
    match rows.next() {
        Some(row) => Ok(row?),
        None => Ok(None),
    }
}

/// Indexes on one relation, which in DuckDB means the ones somebody wrote a
/// `CREATE INDEX` for.
///
/// The primary key is never here. DuckDB maintains keys and UNIQUE constraints
/// with indexes but keeps their details in `duckdb_constraints()`, and its
/// documentation says so. As in `driver-sqlite`: this list is where a front end
/// reads what the planner can use; `ColumnInfo::is_primary_key` is where it
/// reads the key.
pub(crate) fn indexes(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<IndexInfo>, DuckError> {
    // `expressions` is a `VARCHAR` holding DuckDB's rendering of a list —
    // `['(lower(c))', id]` — and casting it back is the catalog unparsing its
    // own value. The alternative is scanning the key list out of the `sql`
    // column the way the SQLite driver has to, which means handling quoted
    // identifiers and commas inside function calls to arrive at an answer
    // DuckDB is holding already.
    let mut stmt = conn.prepare(
        "SELECT index_name, is_unique, CAST(expressions AS VARCHAR[]) \
         FROM duckdb_indexes() \
         WHERE database_name || '.' || schema_name = ? AND table_name = ? \
         ORDER BY index_name",
    )?;
    let rows = stmt.query_map([schema, relation], |row| {
        Ok(IndexInfo {
            name: row.get(0)?,
            is_unique: row.get(1)?,
            columns: strings(row.get(2)?),
            // Read as a constant rather than from the column of the same name,
            // which DuckDB documents as always false. Stating the constant means
            // a version that starts reporting a primary index shows up as a diff
            // here rather than as silently different behaviour.
            is_primary: false,
            // DuckDB's only index type, carried for the reason `driver-sqlite`
            // carries "btree": so the shape does not change per driver for a
            // field the sidebar prints either way.
            method: "art".to_string(),
            // DuckDB refuses `CREATE INDEX … WHERE …` outright — "Creating
            // partial indexes is not supported currently" — so there is no
            // predicate to have, rather than one this driver cannot find.
            predicate: None,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Foreign keys this relation declares.
///
/// One row per key with both sides already ordered, which is where DuckDB's
/// catalog is plainly better than SQLite's — that one reports a column at a time
/// and has to be regrouped.
pub(crate) fn foreign_keys(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT constraint_name, constraint_column_names, referenced_table, referenced_column_names \
         FROM duckdb_constraints() \
         WHERE database_name || '.' || schema_name = ? AND table_name = ? \
           AND constraint_type = 'FOREIGN KEY' \
         ORDER BY constraint_index",
    )?;
    let rows = stmt.query_map([schema, relation], |row| {
        Ok(RelationshipInfo {
            name: row.get(0)?,
            local_columns: strings(row.get(1)?),
            other_schema: schema.to_string(),
            other_table: row.get(2)?,
            other_columns: strings(row.get(3)?),
            on_update: NO_ACTION.to_string(),
            on_delete: NO_ACTION.to_string(),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Foreign keys other relations declare against this one.
///
/// A reverse scan of the same function, and it cannot double-count: DuckDB does
/// keep a mirrored entry on the referenced table, and `duckdb_constraints` skips
/// it deliberately as "already covered by PRIMARY KEY and UNIQUE entries". So
/// each key appears once, on the child, and this is the only way to ask the
/// question from the parent.
///
/// Matching on the table name alone would be ambiguous in a catalog that allowed
/// a key to cross schemas. DuckDB does not — `CREATE TABLE other.child (… REFERENCES app.customers(id))`
/// is refused with "Creating foreign keys across different schemas or catalogs
/// is not supported" — so the declaring table is always in the schema that was
/// asked about, and adding that to the filter makes the match exact rather than
/// merely likely.
pub(crate) fn referenced_by(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, DuckError> {
    // The two column lists are read swapped, so that every field is named for
    // the relation that was asked about rather than for the one that declared
    // the key — the same discipline the PostgreSQL driver's inbound query
    // applies.
    let mut stmt = conn.prepare(
        "SELECT constraint_name, table_name, referenced_column_names, constraint_column_names \
         FROM duckdb_constraints() \
         WHERE database_name || '.' || schema_name = ? AND referenced_table = ? \
           AND constraint_type = 'FOREIGN KEY' \
         ORDER BY table_name, constraint_index",
    )?;
    let rows = stmt.query_map([schema, relation], |row| {
        Ok(RelationshipInfo {
            name: row.get(0)?,
            local_columns: strings(row.get(2)?),
            other_schema: schema.to_string(),
            other_table: row.get(1)?,
            other_columns: strings(row.get(3)?),
            on_update: NO_ACTION.to_string(),
            on_delete: NO_ACTION.to_string(),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// CHECK and UNIQUE constraints, with DuckDB's own rendering of each.
///
/// Where DuckDB beats SQLite outright: SQLite keeps a CHECK constraint only
/// inside the `CREATE TABLE` text, so its driver cannot report one at all.
/// DuckDB puts it in the catalog with `constraint_text`, which is the server's
/// own rendering — the same contract `pg_get_constraintdef` satisfies, and for
/// the same reason: reproducing it from catalog columns means reimplementing
/// expression formatting and getting it subtly wrong on the cases that matter.
///
/// Three of DuckDB's five constraint kinds are left out, each for a stated
/// reason. `PRIMARY KEY` and `FOREIGN KEY` have sections of their own, and
/// listing a key in two places invites the reader to wonder whether they are two
/// different things. `NOT NULL` is already a per-column property in `columns()`;
/// DuckDB models it as a constraint object, and showing it here as well would
/// turn one fact into two.
pub(crate) fn constraints(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<ConstraintInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT constraint_name, constraint_type, constraint_text \
         FROM duckdb_constraints() \
         WHERE database_name || '.' || schema_name = ? AND table_name = ? \
           AND constraint_type IN ('CHECK', 'UNIQUE') \
         ORDER BY constraint_type, constraint_index",
    )?;
    let rows = stmt.query_map([schema, relation], |row| {
        let kind: String = row.get(1)?;
        Ok(ConstraintInfo {
            name: row.get(0)?,
            kind: if kind == "CHECK" {
                ConstraintKind::Check
            } else {
                ConstraintKind::Unique
            },
            definition: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The only referential action DuckDB has.
///
/// Not a placeholder for something unread: DuckDB has no `ON DELETE CASCADE` and
/// no `ON UPDATE` at all, and `information_schema.referential_constraints`
/// documents both rules as always `NO ACTION`. Stating the constant is honest;
/// querying for it would be theatre.
const NO_ACTION: &str = "NO ACTION";

/// A `VARCHAR[]` column as a list of names.
///
/// `duckdb_constraints()` keeps both sides of a key as arrays, already in
/// declaration order, so the composite case needs no grouping pass and no
/// `WITH ORDINALITY`. Anything that is not a string is dropped rather than
/// rendered: these columns hold identifiers and nothing else, and a `?` in a key
/// list would read as a column somebody named `?`.
fn strings(value: Value) -> Vec<String> {
    let (Value::List(items) | Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            Value::Text(name) => Some(name),
            _ => None,
        })
        .collect()
}
