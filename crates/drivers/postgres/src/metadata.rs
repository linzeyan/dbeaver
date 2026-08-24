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
    ColumnInfo, Computed, ConstraintInfo, ConstraintKind, DatabaseInfo, IndexInfo, InfoField,
    RelationInfo, RelationKind, RelationshipInfo, RoutineInfo, RoutineKind, SchemaInfo,
    SequenceInfo, TriggerInfo, UniqueKeyInfo,
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

/// `pg_proc.prokind`, as the two-way split the navigator draws.
///
/// Aggregates (`a`) and window functions (`w`) are functions: they are called in
/// an expression, which is what the distinction is for. Anything else is a
/// version of PostgreSQL that has learned a fourth kind, and calling it a
/// procedure — something a reader would then try to `CALL` — is the wrong guess
/// of the two.
fn routine_kind(c: &str) -> RoutineKind {
    match c.chars().next().unwrap_or(' ') {
        'p' => RoutineKind::Procedure,
        _ => RoutineKind::Function,
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

/// Every schema, with the engine's own marked rather than dropped.
///
/// The `WHERE` clause that used to be here is now the `is_system` column: the
/// same rule, moved from "these do not exist" to "these are the server's". What
/// it buys is a `pg_catalog` somebody can ask to see — the old shape left it out
/// of the answer, so no setting above could put it back.
///
/// Two prefixes and one name. Everything `pg_` is the server's, which catches
/// `pg_catalog`, the per-session `pg_temp_3` and the `pg_toast` schemas in one
/// rule; `information_schema` is the standard's and is nobody's data either. The
/// escape in the pattern is there because `_` is a wildcard in `LIKE` — without
/// it, `pgadmin` would be a system schema.
///
/// The system ones sort last rather than by name. They are the minority nobody
/// asked for, and a tree that opens with `information_schema` above `public`
/// puts the least interesting row first.
pub(crate) async fn schemas(client: &Client) -> Result<Vec<SchemaInfo>, PgError> {
    let rows = client
        .query(
            "SELECT nspname, \
                    (nspname LIKE 'pg\\_%' OR nspname = 'information_schema') AS is_system \
             FROM pg_catalog.pg_namespace \
             ORDER BY (nspname LIKE 'pg\\_%' OR nspname = 'information_schema'), nspname",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| SchemaInfo {
            name: r.get(0),
            is_system: r.get(1),
        })
        .collect())
}

/// Every database this login may open, and which one it is on.
///
/// `datallowconn` excludes the ones no connection is permitted to, and
/// `datistemplate` excludes `template0` and `template1` — offering either as
/// somewhere to open would be offering something that fails.
///
/// `current_database()` is asked of the server rather than read off the
/// connection string, because a string that names no database still lands on
/// one.
pub(crate) async fn databases(client: &Client) -> Result<Vec<DatabaseInfo>, PgError> {
    let rows = client
        .query(
            "SELECT datname, datname = current_database() \
             FROM pg_catalog.pg_database \
             WHERE datallowconn AND NOT datistemplate \
             ORDER BY datname",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| DatabaseInfo {
            name: r.get(0),
            is_current: r.get(1),
        })
        .collect())
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

/// The functions and procedures in a schema, without their bodies.
///
/// `prokind` is PostgreSQL 11 and later. Before that the kind lived in two
/// booleans, `proisagg` and `proiswindow`, and there were no procedures at all —
/// so a server old enough to lack this column has nothing this function could
/// report a procedure on. Reading it as `text` rather than as `"char"` for the
/// reason `relation_kind` gives: the compatibility servers do not agree on which
/// type that column arrives as.
///
/// `pg_get_function_result` answers NULL for a procedure, which is what
/// `RoutineInfo::returns` wants — a procedure has no return clause, and `void`
/// is a function that declares one.
///
/// Ordered by name and then by argument list, so that the overloads of one name
/// sit together and in the same order on every read. A tree that reordered them
/// between refreshes would move rows under the pointer.
pub(crate) async fn routines(client: &Client, schema: &str) -> Result<Vec<RoutineInfo>, PgError> {
    let rows = client
        .query(
            "SELECT p.oid::text, \
                    p.proname, \
                    p.prokind::text, \
                    pg_catalog.pg_get_function_arguments(p.oid), \
                    pg_catalog.pg_get_function_result(p.oid), \
                    l.lanname \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             LEFT JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
             WHERE n.nspname = $1 \
             ORDER BY p.proname, pg_catalog.pg_get_function_arguments(p.oid)",
            &[&schema],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| RoutineInfo {
            schema: schema.to_string(),
            name: r.get(1),
            kind: routine_kind(r.get(2)),
            id: r.get(0),
            arguments: r.get(3),
            returns: r.get(4),
            language: r.get(5),
        })
        .collect())
}

/// What PostgreSQL has to say about one relation.
///
/// Five facts, each left out where the server answers nothing rather than
/// printed as a dash — a row that says "Owner: —" is a row claiming the table
/// has no owner.
///
/// The row estimate is here as well as on the navigator row, and it is here with
/// the thing the navigator has no room for: when it was last taken. `reltuples`
/// is whatever the last ANALYZE saw and every write since has drifted from it,
/// so "≈5,000, analysed 3 days ago" is the answer to "why does the count in the
/// grid not match the sidebar" — which is otherwise a bug report.
///
/// `pg_total_relation_size` rather than `pg_relation_size`: the number somebody
/// wants when they ask how big a table is includes its indexes and its TOAST,
/// which for a wide table is most of it.
pub(crate) async fn table_info(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<InfoField>, PgError> {
    let rows = client
        .query(
            "SELECT pg_catalog.pg_get_userbyid(c.relowner), \
                    pg_catalog.pg_size_pretty(pg_catalog.pg_total_relation_size(c.oid)), \
                    c.relpersistence::text, \
                    pg_catalog.obj_description(c.oid, 'pg_class'), \
                    GREATEST(s.last_analyze, s.last_autoanalyze)::text \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_catalog.pg_stat_all_tables s ON s.relid = c.oid \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &relation],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    let owner: Option<String> = row.get(0);
    if let Some(owner) = owner {
        out.push(InfoField {
            label: "Owner".to_string(),
            value: owner,
        });
    }
    // Null for a view, which occupies nothing: the size of a view is the size of
    // the query, and printing "0 bytes" would say it is an empty table.
    let size: Option<String> = row.get(1);
    if let Some(size) = size {
        out.push(InfoField {
            label: "Size".to_string(),
            value: size,
        });
    }
    // Only when it is not the ordinary kind. Every table is permanent, so a row
    // saying so on every table is a row nobody reads; an unlogged table is not
    // replicated and does not survive a crash, which is worth a line.
    let persistence: Option<String> = row.get(2);
    if let Some(word) = persistence.as_deref().and_then(persistence_word) {
        out.push(InfoField {
            label: "Persistence".to_string(),
            value: word.to_string(),
        });
    }
    let analyzed: Option<String> = row.get(4);
    if let Some(analyzed) = analyzed {
        out.push(InfoField {
            label: "Row estimate taken".to_string(),
            value: analyzed,
        });
    }
    // Last, because it is the only one that can be a paragraph.
    let comment: Option<String> = row.get(3);
    if let Some(comment) = comment {
        out.push(InfoField {
            label: "Comment".to_string(),
            value: comment,
        });
    }
    Ok(out)
}

/// `relpersistence`, or `None` for the permanent tables that are nearly all of
/// them.
fn persistence_word(code: &str) -> Option<&'static str> {
    match code {
        "u" => Some("unlogged — not replicated, and emptied by a crash"),
        "t" => Some("temporary — this session only"),
        _ => None,
    }
}

/// The sequences in one schema, from `pg_sequences`.
///
/// The view rather than `pg_sequence` joined to `pg_class`: it is the one the
/// server documents, it already resolves the schema name, and it answers
/// `last_value` as `NULL` for a sequence this login may see but not read —
/// which is the answer `SequenceInfo::last_value` is `Option` for. Reaching
/// into `pg_sequence` directly would need `SELECT` on every sequence to return
/// any row at all, so a login with partial rights would see none of them
/// instead of seeing them without their current values.
///
/// Everything numeric is cast to text on the server. These are `bigint` here
/// and are not on every engine this file's shape will be copied to, and nothing
/// above does arithmetic on them.
pub(crate) async fn sequences(client: &Client, schema: &str) -> Result<Vec<SequenceInfo>, PgError> {
    let rows = client
        .query(
            "SELECT sequencename, \
                    last_value::text, \
                    increment_by::text, \
                    min_value::text, \
                    max_value::text, \
                    cycle, \
                    cache_size::text \
             FROM pg_catalog.pg_sequences \
             WHERE schemaname = $1 \
             ORDER BY sequencename",
            &[&schema],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| SequenceInfo {
            schema: schema.to_string(),
            name: r.get(0),
            last_value: r.get(1),
            increment: r.get(2),
            min_value: r.get(3),
            max_value: r.get(4),
            cycles: r.get(5),
            cache: r.get(6),
        })
        .collect())
}

/// One routine's source, as the server renders it.
///
/// The oid is compared as text rather than cast to `oid`, because the id came
/// back from `routines` as a string and a cast is the one place a stale or
/// mistyped one would fail as a *statement* error rather than as "no such
/// routine". `pg_proc` is small enough that giving up the index costs nothing
/// measurable.
///
/// Restricted to `f` and `p`: `pg_get_functiondef` raises an error on an
/// aggregate rather than answering, so asking it about one would turn a routine
/// somebody clicked into a failed read. `None` is the honest answer there — an
/// aggregate's definition is its transition and final functions, which are two
/// other rows in this list.
pub(crate) async fn routine_definition(
    client: &Client,
    schema: &str,
    id: &str,
) -> Result<Option<String>, PgError> {
    let rows = client
        .query(
            "SELECT pg_catalog.pg_get_functiondef(p.oid) \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.oid::text = $2 AND p.prokind IN ('f', 'p')",
            &[&schema, &id],
        )
        .await?;
    Ok(rows.first().map(|r| r.get(0)))
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
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
                    a.attgenerated \
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
            // `pg_get_expr` hands back a `GENERATED ALWAYS AS (…)` expression in
            // exactly the shape it hands back a default, so this column is the
            // only thing that tells them apart. Without it the renderer wrote
            // `b int4 DEFAULT (a * 2)`, which PostgreSQL refuses outright — a
            // default may not reference another column.
            //
            // `"char"` rather than `text`: an empty string for an ordinary
            // column, `s` for one PostgreSQL stores, `v` for one it evaluates on
            // read. Anything else is a version of PostgreSQL that has learned a
            // third arrangement, and calling it a default would be the same
            // mistake in a new form.
            computed: match r.get::<_, i8>(6) as u8 {
                b's' => Some(Computed::Stored),
                b'v' => Some(Computed::Virtual),
                _ => None,
            },
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

/// UNIQUE constraints that name columns, primary key excluded.
///
/// `pg_index` rather than `pg_constraint`, because `CREATE UNIQUE INDEX` makes a
/// key just as `UNIQUE (…)` does and PostgreSQL only records the second one as a
/// constraint. Reading the first list would leave a table whose only key was
/// created as an index looking like a table with no key at all.
///
/// Three conditions do the filtering `UniqueKeyInfo` describes. `indpred IS
/// NULL` drops the partial index, which is unique over the rows it covers and
/// says nothing about the rest. `indexprs IS NULL` drops the expression index,
/// whose keys are not columns. `indnkeyatts = indnatts` drops the one with an
/// `INCLUDE` list, so a payload column is not mistaken for part of the key.
pub(crate) async fn unique_keys(
    client: &Client,
    schema: &str,
    relation: &str,
) -> Result<Vec<UniqueKeyInfo>, PgError> {
    // The columns come from indkey with ordinality rather than from a plain join
    // against pg_attribute, because attnum order is not key order and a
    // composite key read in the wrong order is a WHERE clause that pairs each
    // value with the wrong column.
    let rows = client
        .query(
            "SELECT i.relname, \
                    ARRAY(SELECT a.attname \
                          FROM unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) \
                          JOIN pg_catalog.pg_attribute a \
                            ON a.attrelid = ix.indrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord) \
             FROM pg_catalog.pg_index ix \
             JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_catalog.pg_class c ON c.oid = ix.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND ix.indisunique AND NOT ix.indisprimary \
               AND ix.indpred IS NULL AND ix.indexprs IS NULL \
               AND ix.indnkeyatts = ix.indnatts \
             ORDER BY i.relname",
            &[&schema, &relation],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| UniqueKeyInfo {
            name: r.get(0),
            columns: r.get(1),
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
