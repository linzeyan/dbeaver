//! Schema introspection for the navigator sidebar.
//!
//! Queries `pg_catalog` directly rather than `information_schema`. The latter
//! is portable but implemented as views over the former, and is markedly slower
//! on databases with many objects — which is exactly where a sidebar has to
//! stay responsive.
//!
//! Unlike result data, metadata is small and crosses the FFI boundary as JSON.
//! Arrow buys nothing for a few thousand short rows, and JSON keeps the Swift
//! side trivial.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo,
};
use tokio_postgres::Client;

use tokio_postgres::error::SqlState;

use crate::PgError;

/// `pg_class.relkind`, as one of the kinds the navigator knows about.
///
/// A free function rather than a method: the enum belongs to `dbconn` now, so
/// that every driver's navigator entries mean the same thing.
/// `pg_class.relkind`, read as text.
///
/// PostgreSQL types this column as `"char"`, a one-byte type that arrives as an
/// `i8`. Databases that serve `pg_catalog` for compatibility do not all agree:
/// GreptimeDB returns it as a string, and reading it as `i8` failed on the
/// first table in the list. The queries cast it to `text`, which both answer the
/// same way, so the driver reads one type instead of guessing which it got.
fn relation_kind(c: &str) -> RelationKind {
    match c.chars().next().unwrap_or(' ') {
        'r' => RelationKind::Table,
        'v' => RelationKind::View,
        'm' => RelationKind::MaterializedView,
        'f' => RelationKind::ForeignTable,
        'p' => RelationKind::PartitionedTable,
        _ => RelationKind::Unknown,
    }
}

/// `pg_constraint.contype`, for the constraints that get their own section.
fn constraint_kind(c: i8) -> ConstraintKind {
    match c as u8 as char {
        'c' => ConstraintKind::Check,
        'u' => ConstraintKind::Unique,
        'x' => ConstraintKind::Exclude,
        _ => ConstraintKind::Other,
    }
}

/// Decodes `pg_trigger.tgtype`, a documented bitmask.
///
/// Read from the bitmask rather than parsed out of `pg_get_triggerdef`: the
/// definition is a rendered sentence, and picking words out of it is guesswork
/// that breaks on the first unusual trigger.
fn trigger_shape(tgtype: i16) -> (String, Vec<String>, String) {
    const ROW: i16 = 1 << 0;
    const BEFORE: i16 = 1 << 1;
    const INSERT: i16 = 1 << 2;
    const DELETE: i16 = 1 << 3;
    const UPDATE: i16 = 1 << 4;
    const TRUNCATE: i16 = 1 << 5;
    const INSTEAD: i16 = 1 << 6;

    let timing = if tgtype & INSTEAD != 0 {
        "INSTEAD OF"
    } else if tgtype & BEFORE != 0 {
        "BEFORE"
    } else {
        "AFTER"
    };

    let events = [
        (INSERT, "INSERT"),
        (UPDATE, "UPDATE"),
        (DELETE, "DELETE"),
        (TRUNCATE, "TRUNCATE"),
    ]
    .iter()
    .filter(|(bit, _)| tgtype & bit != 0)
    .map(|(_, name)| name.to_string())
    .collect();

    let level = if tgtype & ROW != 0 {
        "ROW"
    } else {
        "STATEMENT"
    };
    (timing.to_string(), events, level.to_string())
}

/// `confupdtype`/`confdeltype` spelled the way the DDL spells them.
fn referential_action(c: i8) -> String {
    match c as u8 as char {
        'r' => "RESTRICT",
        'c' => "CASCADE",
        'n' => "SET NULL",
        'd' => "SET DEFAULT",
        // 'a' is the default, and writing it out on every row is noise.
        _ => "NO ACTION",
    }
    .to_string()
}

pub(crate) async fn schemas(client: &Client) -> Result<Vec<SchemaInfo>, PgError> {
    // Excludes catalog and toast schemas; `pg_temp`/`pg_toast_temp` are matched
    // by the same prefix.
    let rows = client
        .query(
            "SELECT nspname \
             FROM pg_catalog.pg_namespace \
             WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
             ORDER BY nspname",
            &[],
        )
        .await?;
    Ok(rows.iter().map(|r| SchemaInfo { name: r.get(0) }).collect())
}

pub(crate) async fn relations(client: &Client, schema: &str) -> Result<Vec<RelationInfo>, PgError> {
    let rows = client
        .query(
            "SELECT c.relname, c.relkind::text, c.reltuples::bigint \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'v', 'm', 'f', 'p') \
             ORDER BY c.relname",
            &[&schema],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| {
            // Optional, and not because PostgreSQL ever leaves it null.
            // CockroachDB does: it serves `pg_catalog` for compatibility and
            // fills in what it has, and a row count it does not track is null
            // rather than a number. Reading straight into `i64` turned that into
            // a deserialization failure, so listing the tables of a
            // CockroachDB database failed outright — over a column nothing
            // needs.
            let estimated: Option<i64> = r.get(2);
            RelationInfo {
                schema: schema.to_string(),
                name: r.get(0),
                kind: relation_kind(r.get(1)),
                // -1 when the relation has never been analyzed. Clamping that to
                // zero, as this used to, reported a full table as empty — and it
                // took a second driver with the same hole to notice, because a
                // benchmark database is analyzed and never showed it.
                estimated_rows: estimated.filter(|n| *n >= 0),
            }
        })
        .collect())
}

/// A column's place in the relation, and the types it is read as.
///
/// `int4` rather than `int` in the casts below, which reads as pedantry until
/// the driver meets CockroachDB: there, `INT` is 64-bit by default, so
/// `attnum::int` comes back as int8 and deserializing it into an `i32` fails.
/// Naming the width means the same statement asks for the same type everywhere.
pub(crate) async fn columns(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<ColumnInfo>, PgError> {
    let rows = client
        .query(
            "SELECT a.attname, \
                    pg_catalog.format_type(a.atttypid, a.atttypmod), \
                    NOT a.attnotnull, \
                    a.attnum::int4, \
                    COALESCE(pk.indisprimary, false), \
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_catalog.pg_attrdef d \
                    ON d.adrelid = c.oid AND d.adnum = a.attnum \
             LEFT JOIN pg_catalog.pg_index pk \
                    ON pk.indrelid = c.oid AND pk.indisprimary \
                   AND a.attnum = ANY(pk.indkey) \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&schema, &relation],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            name: r.get(0),
            data_type: r.get(1),
            nullable: r.get(2),
            position: r.get(3),
            is_primary_key: r.get(4),
            default_value: r.get(5),
            // Not read here, and the field above is the poorer for it: a
            // `GENERATED ALWAYS AS (…) STORED` column keeps its expression in
            // `pg_attrdef` like any default, and only `pg_attribute.attgenerated`
            // tells the two apart — a column this query does not ask for. So a
            // generated column arrives looking like a column with a default,
            // which is the same confusion SQL Server's driver just stopped
            // making.
            computed: None,
        })
        .collect())
}

/// The statement a view or materialized view is defined by, as the server
/// renders it; `None` for anything else.
///
/// Absent rather than empty for a table, which is the distinction the UI hangs
/// a section on. `pg_get_viewdef` does not object to a table's oid — it returns
/// an empty string — so without the relkind filter every table would report a
/// definition it does not have, and the pane would show an empty box instead of
/// no box.
pub(crate) async fn definition(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Option<String>, PgError> {
    let rows = client
        .query(
            "SELECT pg_catalog.pg_get_viewdef(c.oid, true) \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('v', 'm')",
            &[&schema, &relation],
        )
        .await?;
    Ok(rows.first().map(|r| r.get(0)))
}

pub(crate) async fn indexes(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<IndexInfo>, PgError> {
    // Key expressions come from pg_get_indexdef one position at a time rather
    // than by joining indkey against pg_attribute: that join silently drops
    // expression keys, which appear in indkey as attnum 0.
    let rows = client
        .query(
            "SELECT i.relname, \
                    ix.indisunique, \
                    ix.indisprimary, \
                    am.amname, \
                    pg_catalog.pg_get_expr(ix.indpred, ix.indrelid), \
                    ARRAY(SELECT pg_catalog.pg_get_indexdef(ix.indexrelid, k::int4, true) \
                          FROM generate_series(1, ix.indnkeyatts) AS k \
                          ORDER BY k) \
             FROM pg_catalog.pg_index ix \
             JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_catalog.pg_class c ON c.oid = ix.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_am am ON am.oid = i.relam \
             WHERE n.nspname = $1 AND c.relname = $2 \
             ORDER BY ix.indisprimary DESC, i.relname",
            &[&schema, &relation],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| IndexInfo {
            name: r.get(0),
            is_unique: r.get(1),
            is_primary: r.get(2),
            method: r.get(3),
            predicate: r.get(4),
            columns: r.get(5),
        })
        .collect())
}

/// Foreign keys this relation declares.
pub(crate) async fn foreign_keys(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, PgError> {
    relationships(client, schema, relation, Direction::Outbound).await
}

/// Foreign keys other relations declare against this one.
pub(crate) async fn referenced_by(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, PgError> {
    relationships(client, schema, relation, Direction::Inbound).await
}

enum Direction {
    Outbound,
    Inbound,
}

async fn relationships(
    client: &Client,
    schema: &str,
    relation: &str,
    direction: Direction,
) -> Result<Vec<RelationshipInfo>, PgError> {
    // Two statements rather than one parameterised by direction: the sides
    // swap in three places, and a query that switches which array is "local"
    // based on a flag is one edit away from reporting a key backwards.
    //
    // WITH ORDINALITY on both key arrays, because a composite key's columns
    // have to line up with the ones they reference, and attnum order is not
    // that order.
    let sql = match direction {
        Direction::Outbound => {
            "SELECT con.conname, \
                    ARRAY(SELECT a.attname \
                          FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
                          JOIN pg_catalog.pg_attribute a \
                            ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord), \
                    fn.nspname, f.relname, \
                    ARRAY(SELECT a.attname \
                          FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord) \
                          JOIN pg_catalog.pg_attribute a \
                            ON a.attrelid = con.confrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord), \
                    con.confupdtype, con.confdeltype \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_class f ON f.oid = con.confrelid \
             JOIN pg_catalog.pg_namespace fn ON fn.oid = f.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND con.contype = 'f' \
             ORDER BY con.conname"
        }
        Direction::Inbound => {
            "SELECT con.conname, \
                    ARRAY(SELECT a.attname \
                          FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord) \
                          JOIN pg_catalog.pg_attribute a \
                            ON a.attrelid = con.confrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord), \
                    n.nspname, c.relname, \
                    ARRAY(SELECT a.attname \
                          FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
                          JOIN pg_catalog.pg_attribute a \
                            ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord), \
                    con.confupdtype, con.confdeltype \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_class f ON f.oid = con.confrelid \
             JOIN pg_catalog.pg_namespace fn ON fn.oid = f.relnamespace \
             WHERE fn.nspname = $1 AND f.relname = $2 AND con.contype = 'f' \
             ORDER BY n.nspname, c.relname, con.conname"
        }
    };

    let rows = client.query(sql, &[&schema, &relation]).await?;
    Ok(rows
        .iter()
        .map(|r| RelationshipInfo {
            name: r.get(0),
            local_columns: r.get(1),
            other_schema: r.get(2),
            other_table: r.get(3),
            other_columns: r.get(4),
            on_update: referential_action(r.get(5)),
            on_delete: referential_action(r.get(6)),
        })
        .collect())
}

pub(crate) async fn constraints(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<ConstraintInfo>, PgError> {
    // Primary keys and foreign keys are excluded: both already have their own
    // section, and listing a key in two places invites the reader to wonder
    // whether they are two different things.
    let rows = client
        .query(
            "SELECT con.conname, \
                    con.contype, \
                    pg_catalog.pg_get_constraintdef(con.oid, true) \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND con.contype IN ('c', 'u', 'x') \
             ORDER BY con.contype, con.conname",
            &[&schema, &relation],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| ConstraintInfo {
            name: r.get(0),
            kind: constraint_kind(r.get(1)),
            definition: r.get(2),
        })
        .collect())
}

pub(crate) async fn triggers(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<TriggerInfo>, PgError> {
    // `NOT tgisinternal` is what keeps this list to triggers someone wrote.
    // Every foreign key installs a pair of enforcement triggers, so without
    // the filter bench_child would report constraint machinery as user code.
    let rows = match client
        .query(
            "SELECT t.tgname, t.tgtype, t.tgenabled, p.proname, \
                    pg_catalog.pg_get_triggerdef(t.oid, true) \
             FROM pg_catalog.pg_trigger t \
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
             WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal \
             ORDER BY t.tgname",
            &[&schema, &relation],
        )
        .await
    {
        Ok(rows) => rows,
        // A database serving `pg_catalog` without `pg_get_triggerdef` has no
        // triggers to describe. CockroachDB is the case: it provides the
        // catalog for compatibility and simply does not implement the function,
        // so the whole structure pane failed on a table that has no triggers
        // either way.
        //
        // Matched on the SQLSTATE for undefined_function and nothing else. A
        // blanket catch here would turn a genuinely broken catalog query into
        // "this table has no triggers", which is the kind of empty answer
        // nobody investigates.
        Err(e)
            if e.as_db_error()
                .is_some_and(|db| *db.code() == SqlState::UNDEFINED_FUNCTION) =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.into()),
    };

    Ok(rows
        .iter()
        .map(|r| {
            let (timing, events, level) = trigger_shape(r.get(1));
            let enabled: i8 = r.get(2);
            TriggerInfo {
                name: r.get(0),
                // Optional in the shared shape because SQLite records none of
                // these; PostgreSQL keeps all four in columns and states them.
                timing: Some(timing),
                events,
                level: Some(level),
                function: Some(r.get(3)),
                // 'D' is disabled; 'O', 'R' and 'A' all fire in some session.
                enabled: enabled as u8 as char != 'D',
                definition: r.get(4),
            }
        })
        .collect())
}
