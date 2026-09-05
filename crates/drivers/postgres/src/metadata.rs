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
    ColumnInfo, Computed, ConstraintInfo, ConstraintKind, DatabaseInfo, EndProcess, IndexInfo,
    InfoField, ProcessInfo, RelationInfo, RelationKind, RelationshipInfo, RoutineInfo, RoutineKind,
    SchemaInfo, SequenceInfo, TriggerInfo, UniqueKeyInfo, VariableInfo, VariableScope,
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
/// so a date beside the "≈5,000" in the tree is the answer to "why does the
/// count in the grid not match the sidebar" — which is otherwise a bug report.
/// Truncated to the second, because the question it answers is how stale the
/// estimate is and microseconds are six digits of noise across that.
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
                    CASE WHEN c.relkind IN ('v', 'f') THEN NULL \
                         ELSE pg_catalog.pg_size_pretty( \
                                  pg_catalog.pg_total_relation_size(c.oid)) END, \
                    c.relpersistence::text, \
                    pg_catalog.obj_description(c.oid, 'pg_class'), \
                    date_trunc('second', GREATEST(s.last_analyze, s.last_autoanalyze))::text \
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
    // Asked for only where there is storage to measure. `pg_total_relation_size`
    // answers 0 for a view rather than null, and "Size: 0 bytes" under a view
    // reads as a table that has been emptied — the size of a view is the size of
    // the query behind it, which is a thing the DDL section shows.
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

/// Who this connection is, and what the server already decided it may do.
///
/// Two queries and not one, because the two halves are not equally portable.
/// `current_user`, `session_user` and `current_database()` are SQL that every
/// database serving this wire protocol answers; `pg_roles` is a `pg_catalog`
/// view, and this file's header says what that distinction is worth here —
/// CockroachDB and GreptimeDB reach this driver too, and a single query would
/// make the name somebody actually came for depend on a catalog view they may
/// not serve. A failure reading the attributes leaves the identity standing.
///
/// The privilege rows are asked of the server rather than derived from the role
/// attributes, because `has_database_privilege` is the same question the server
/// will answer when the statement is sent: a role inherits CREATE through a
/// grant to a group it is a member of, and a client that read `rolsuper` and
/// stopped would tell somebody they cannot create a table moments before they
/// do.
pub(crate) async fn login_info(client: &Client) -> Result<Vec<InfoField>, PgError> {
    let rows = client
        .query(
            "SELECT current_user::text, session_user::text, current_database()",
            &[],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    let current: String = row.get(0);
    let session: String = row.get(1);
    // Only when they differ, which is only after a SET ROLE. Equal is the
    // ordinary case, and a second row repeating the first would push the
    // privileges below the fold to say nothing.
    let assumed = session != current;
    out.push(InfoField {
        label: "Connected as".to_string(),
        value: current,
    });
    if assumed {
        out.push(InfoField {
            label: "Logged in as".to_string(),
            value: session,
        });
    }
    // `current_database()` is NULL on a connection with no database, which is
    // not something this client opens but is something a pooler can hand back.
    let database: Option<String> = row.get(2);
    if let Some(database) = database {
        out.push(InfoField {
            label: "Database".to_string(),
            value: database,
        });
    }

    let Ok(rows) = client
        .query(
            "SELECT r.rolsuper, r.rolcreatedb, r.rolcreaterole, r.rolreplication, \
                    (SELECT string_agg(g.rolname, ', ' ORDER BY g.rolname) \
                       FROM pg_catalog.pg_auth_members m \
                       JOIN pg_catalog.pg_roles g ON g.oid = m.roleid \
                      WHERE m.member = r.oid), \
                    has_database_privilege(current_database(), 'CONNECT'), \
                    has_database_privilege(current_database(), 'CREATE'), \
                    has_database_privilege(current_database(), 'TEMPORARY') \
             FROM pg_catalog.pg_roles r \
             WHERE r.rolname = current_user",
            &[],
        )
        .await
    else {
        return Ok(out);
    };
    let Some(row) = rows.first() else {
        return Ok(out);
    };

    // Named in the order they widen what somebody can do, and joined into one
    // row rather than four. Four rows of "no" is a wall of nothing; one row
    // saying "none" is the same answer read at a glance.
    let attributes = [
        (row.get::<_, bool>(0), "superuser"),
        (row.get::<_, bool>(1), "create database"),
        (row.get::<_, bool>(2), "create role"),
        (row.get::<_, bool>(3), "replication"),
    ];
    let held: Vec<&str> = attributes
        .iter()
        .filter(|(has, _)| *has)
        .map(|(_, word)| *word)
        .collect();
    out.push(InfoField {
        label: "Role attributes".to_string(),
        value: if held.is_empty() {
            "none".to_string()
        } else {
            held.join(", ")
        },
    });

    let member_of: Option<String> = row.get(4);
    if let Some(member_of) = member_of {
        out.push(InfoField {
            label: "Member of".to_string(),
            value: member_of,
        });
    }

    // CONNECT is included even though holding this connection proves it: it is
    // revocable, and a session that outlived the revocation is exactly when
    // somebody opens this sheet.
    let on_database = [
        (row.get::<_, Option<bool>>(5), "connect"),
        (row.get::<_, Option<bool>>(6), "create"),
        (row.get::<_, Option<bool>>(7), "temporary tables"),
    ];
    let granted: Vec<&str> = on_database
        .iter()
        .filter(|(has, _)| *has == Some(true))
        .map(|(_, word)| *word)
        .collect();
    if !granted.is_empty() {
        out.push(InfoField {
            label: "On this database".to_string(),
            value: granted.join(", "),
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

/// Everything `pg_stat_activity` can see, this client's own connections
/// included.
///
/// Unfiltered, under the rule the trait states: the background workers are in
/// here beside the client backends, and so are the idle connections. A reader
/// looking for what is holding a lock wants the idle-in-transaction rows most
/// of all, and they are exactly what a "show me the busy ones" filter would
/// drop.
///
/// The duration is time in the current *state* rather than time connected.
/// For an `active` backend those are the same question — `state_change` is when
/// the statement started — and for the row that matters most they are not: an
/// `idle in transaction` connection may have been dialled this morning and gone
/// idle four minutes ago, and four minutes is the number somebody deciding
/// whether to end it needs.
///
/// A backend with no visible query is not a mistake. `pg_stat_activity` shows
/// the statement text only to a superuser or to a member of
/// `pg_read_all_stats`, and to the owner of the backend; everybody else reads
/// the row with an empty `query`. The list is still worth drawing — who is
/// connected, from where, and in what state, is most of it.
pub(crate) async fn processes(client: &Client) -> Result<Vec<ProcessInfo>, PgError> {
    let rows = client
        .query(
            "SELECT pid::text, \
                    coalesce(usename, ''), \
                    coalesce(datname, ''), \
                    coalesce(state, backend_type, ''), \
                    coalesce(date_trunc('second', now() - state_change)::text, ''), \
                    coalesce(query, '') \
             FROM pg_catalog.pg_stat_activity \
             ORDER BY pid",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| ProcessInfo {
            id: row.get(0),
            user: row.get(1),
            database: row.get(2),
            state: row.get(3),
            duration: row.get(4),
            statement: row.get(5),
        })
        .collect())
}

/// `pg_cancel_backend` or `pg_terminate_backend`, by the id `processes` gave.
///
/// Both answer `false` for a pid that is not there, which is the answer this
/// wants: a backend that ended between the list being drawn and a row being
/// chosen has already done what was being asked of it. They also answer `false`
/// without the privilege to signal that backend — `pg_signal_backend` or
/// ownership — and the caller cannot tell the two apart from here. That is the
/// server's own reporting and not something to improve on by guessing: what a
/// front end does either way is refresh the list, which shows which it was.
pub(crate) async fn end_process(
    client: &Client,
    id: &str,
    how: EndProcess,
) -> Result<bool, PgError> {
    // The id came from `processes` and is a pid, so anything else is a caller
    // that made one up. Refused by name rather than passed to the server, which
    // would report it as a type error about `$1`.
    let pid: i32 = id
        .parse()
        .map_err(|_| PgError::NotABackend(id.to_string()))?;
    let statement = match how {
        EndProcess::Statement => "SELECT pg_catalog.pg_cancel_backend($1)",
        EndProcess::Session => "SELECT pg_catalog.pg_terminate_backend($1)",
    };
    let row = client.query_one(statement, &[&pid]).await?;
    Ok(row.get::<_, Option<bool>>(0).unwrap_or(false))
}

/// Every setting in `pg_settings`, with the scope read off `source`.
///
/// `pg_settings` rather than `SHOW ALL`, which returns the same names and values
/// and one column of prose instead of the one that says where each value came
/// from. The description is not shown — six hundred paragraphs is a manual, and
/// PostgreSQL's is better and already written.
///
/// A setting no role may read comes back with `setting` null rather than as an
/// error, and is listed with a blank value: the name is still a true fact about
/// the server, and dropping the row would make a filter for it come back empty
/// as though the setting did not exist.
///
/// `source` is coalesced for a different reason. PostgreSQL always fills it in;
/// CockroachDB, which this driver also reaches, serves `pg_settings` for
/// compatibility and leaves the column null for every row — and a null read as
/// `&str` panics inside the client rather than failing the call. `scope_of`
/// answers `Server` for a source it does not recognise, which is the right
/// answer for a server that does not say.
///
/// Sorted here and not by the server, which is the one thing this does not ask
/// PostgreSQL for. `ORDER BY name` sorts in the database's collation, and
/// `en_US.UTF-8` ignores the underscores — which puts `logging_collector` in the
/// middle of the `log_` settings and `DateStyle` between `data_sync_retry` and
/// `deadlock_timeout`. Byte order keeps every `log_` prefix together, and it is
/// the order MySQL's answer already arrives in, so one rule in `contract.rs`
/// covers both.
pub(crate) async fn variables(client: &Client) -> Result<Vec<VariableInfo>, PgError> {
    let rows = client
        .query(
            "SELECT name, coalesce(setting, ''), coalesce(source, '') \
             FROM pg_catalog.pg_settings",
            &[],
        )
        .await?;
    let mut variables: Vec<VariableInfo> = rows
        .iter()
        .map(|row| VariableInfo {
            name: row.get(0),
            value: row.get(1),
            scope: scope_of(row.get(2)),
        })
        .collect();
    variables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(variables)
}

/// Which of the two scopes a `pg_settings.source` describes.
///
/// PostgreSQL names thirteen sources, and they answer a finer question than
/// `VariableScope` asks: not only whether this value is the server's but which
/// file or statement put it there. The four folded into `Session` are the ones
/// that arrived with this particular connection — `SET` in the session,
/// `PGOPTIONS` in its startup packet, and the per-role and per-database defaults
/// `ALTER ROLE ... SET` and `ALTER DATABASE ... SET` leave behind.
///
/// The last two are the arguable ones, and they are here rather than under
/// `Server` because of what the answer is for: somebody reading this list wants
/// to know whether the value in front of them is the one everybody gets. A
/// setting that follows a role around is not, and calling it the server's would
/// send them to `postgresql.conf` to look for something that was never in it.
///
/// Anything unrecognised is the server's. A source this does not know is far
/// more likely to be a new way of writing configuration than a new way for one
/// connection to differ from the rest, and the wrong guess in that direction is
/// the quieter one — it says "everybody has this" about a value that is in fact
/// only yours, rather than sending somebody looking for a per-session override
/// that does not exist.
fn scope_of(source: &str) -> VariableScope {
    match source {
        "session" | "client" | "user" | "database" | "database user" => VariableScope::Session,
        _ => VariableScope::Server,
    }
}
