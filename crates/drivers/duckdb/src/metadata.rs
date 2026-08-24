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
//! ## Two levels, and which one is showing
//!
//! DuckDB names a relation in three parts — database, schema, table — and the
//! trait's metadata methods carry two. The level above is `databases()`, and the
//! session is on exactly one of them at a time: every query below is filtered by
//! the name `DuckSource::current_database` is holding, and `use_database` is
//! what moves it.
//!
//! This replaced a flattening. A schema used to be reported as `database.schema`
//! — `warehouse.main`, `warehouse.app` — so that an `ATTACH` did not produce two
//! nodes called `main` with different contents. The composite read correctly and
//! pasted into SQL correctly, and it was still the wrong shape: it put DuckDB's
//! database level in the one place a front end cannot draw a level, and it made
//! the common case, one database with one schema, read `memory.main` where every
//! other driver reads `main`.
//!
//! What the level costs in exchange: a table in an attached database is not in
//! the tree until the session is moved onto it. Everything is reachable, and no
//! longer all at once.
//!
//! The database is bound as a parameter here rather than qualified into the
//! statement, so a database called `sales.2024` needs no quoting rules of its
//! own — the same property the composite name had, kept.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DatabaseInfo, IndexInfo, RelationInfo,
    RelationKind, RelationshipInfo, SchemaInfo, UniqueKeyInfo,
};
use duckdb::Connection;
use duckdb::types::Value;

use crate::DuckError;

/// Every database attached to this connection, and which one the session is on.
///
/// `system` and `temp` are left out for the reason `schemas` leaves them out:
/// one holds DuckDB's own catalog and the other holds a single connection's
/// temporary objects, and neither is somewhere to point a navigator.
///
/// Which one is current is passed in rather than read from `current_database()`
/// here. This query runs on a connection cloned for it, and `USE` is per
/// connection — so the answer that function gives on this connection is the
/// database the *clone* opened on, which is precisely the fact this driver
/// stopped relying on.
pub(crate) fn databases(conn: &Connection, current: &str) -> Result<Vec<DatabaseInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT database_name \
         FROM duckdb_databases() \
         WHERE database_name NOT IN ('system', 'temp') \
         ORDER BY database_name",
    )?;
    let rows = stmt.query_map([], |row| {
        let name: String = row.get(0)?;
        Ok(DatabaseInfo {
            is_current: name == current,
            name,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The schemas of the database this session is on.
///
/// Filtered by database rather than by the `internal` flag, which is the trap
/// here. `duckdb_schemas()` reports `internal = true` for the `main` schema of
/// the user's own database — the schema everything they created is in — so the
/// obvious `WHERE NOT internal` returns a navigator with the user's tables
/// missing and their extra schemas present.
pub(crate) fn schemas(conn: &Connection, database: &str) -> Result<Vec<SchemaInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT schema_name \
         FROM duckdb_schemas() \
         WHERE database_name = ? \
         ORDER BY schema_name",
    )?;
    // By name and not by `internal`, which is the same trap the doc comment
    // above describes: `main` is flagged internal and is the one schema
    // everything the user made is in. The two that really are the engine's are
    // named, and every DuckDB database has exactly these two.
    let rows = stmt.query_map([database], |row| {
        let name: String = row.get(0)?;
        Ok(SchemaInfo {
            is_system: name == "information_schema" || name == "pg_catalog",
            name,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Tables and views in one schema.
///
/// Two functions rather than `information_schema.tables`, which merges them and
/// loses `estimated_size`. There is no third kind to look for: DuckDB has no
/// materialized views, no foreign tables and no partitioned tables, and its
/// table functions are functions rather than relations in the catalog, so
/// SQLite's `Virtual` has nothing to describe here either.
pub(crate) fn relations(
    conn: &Connection,
    database: &str,
    schema: &str,
) -> Result<Vec<RelationInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT table_name AS name, 'table' AS kind, estimated_size \
         FROM duckdb_tables() \
         WHERE database_name = ? AND schema_name = ? AND NOT internal \
         UNION ALL \
         SELECT view_name, 'view', CAST(NULL AS BIGINT) \
         FROM duckdb_views() \
         WHERE database_name = ? AND schema_name = ? AND NOT internal \
         ORDER BY name",
    )?;
    let rows = stmt.query_map([database, schema, database, schema], |row| {
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
    database: &str,
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
         WHERE c.database_name = ? AND c.schema_name = ? AND c.table_name = ? \
         ORDER BY c.column_index",
    )?;
    let rows = stmt.query_map([database, schema, relation], |row| {
        Ok(ColumnInfo {
            name: row.get(0)?,
            data_type: row.get(1)?,
            nullable: row.get(2)?,
            // Documented as one-based and it is, so unlike SQLite's `cid` there
            // is nothing to shift.
            position: row.get(3)?,
            default_value: row.get(4)?,
            computed: None,
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
    database: &str,
    schema: &str,
    relation: &str,
) -> Result<Option<String>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM duckdb_views() \
         WHERE database_name = ? AND schema_name = ? AND view_name = ?",
    )?;
    let mut rows = stmt.query_map([database, schema, relation], |row| {
        row.get::<_, Option<String>>(0)
    })?;
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
    database: &str,
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
         WHERE database_name = ? AND schema_name = ? AND table_name = ? \
         ORDER BY index_name",
    )?;
    let rows = stmt.query_map([database, schema, relation], |row| {
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

/// UNIQUE constraints that name columns, primary key excluded.
///
/// `duckdb_constraints()` for the reason `columns` reads the key from there:
/// DuckDB maintains a UNIQUE constraint with an index but keeps what it is over
/// in the constraint list, and `duckdb_indexes()` shows the index without the
/// columns. It is also the reason nothing has to be filtered out here — DuckDB
/// refuses a partial index outright, and a UNIQUE constraint is over columns by
/// construction, so the two cases `UniqueKeyInfo` warns about cannot arise.
///
/// The cost of reading only this list is that a `CREATE UNIQUE INDEX` is not
/// here: DuckDB records it as an index and never as a constraint, and the index
/// list gives its keys as rendered expressions rather than as column names. A
/// table whose only uniqueness was created that way is therefore not editable,
/// which is the same answer this gives to any key it cannot state as columns —
/// and a smaller price than picking names out of `['(lower(c))', id]`.
///
/// One row per constraint with the columns already in declaration order, so
/// there is no grouping pass.
pub(crate) fn unique_keys(
    conn: &Connection,
    database: &str,
    schema: &str,
    relation: &str,
) -> Result<Vec<UniqueKeyInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT constraint_name, CAST(constraint_column_names AS VARCHAR[]) \
         FROM duckdb_constraints() \
         WHERE database_name = ? AND schema_name = ? AND table_name = ? \
           AND constraint_type = 'UNIQUE' \
         ORDER BY constraint_name, constraint_index",
    )?;
    let rows = stmt.query_map([database, schema, relation], |row| {
        Ok(UniqueKeyInfo {
            name: row.get(0)?,
            columns: strings(row.get(1)?),
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
    database: &str,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT constraint_name, constraint_column_names, referenced_table, referenced_column_names \
         FROM duckdb_constraints() \
         WHERE database_name = ? AND schema_name = ? AND table_name = ? \
           AND constraint_type = 'FOREIGN KEY' \
         ORDER BY constraint_index",
    )?;
    let rows = stmt.query_map([database, schema, relation], |row| {
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
    database: &str,
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
         WHERE database_name = ? AND schema_name = ? AND referenced_table = ? \
           AND constraint_type = 'FOREIGN KEY' \
         ORDER BY table_name, constraint_index",
    )?;
    let rows = stmt.query_map([database, schema, relation], |row| {
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
    database: &str,
    schema: &str,
    relation: &str,
) -> Result<Vec<ConstraintInfo>, DuckError> {
    let mut stmt = conn.prepare(
        "SELECT constraint_name, constraint_type, constraint_text \
         FROM duckdb_constraints() \
         WHERE database_name = ? AND schema_name = ? AND table_name = ? \
           AND constraint_type IN ('CHECK', 'UNIQUE') \
         ORDER BY constraint_type, constraint_index",
    )?;
    let rows = stmt.query_map([database, schema, relation], |row| {
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
