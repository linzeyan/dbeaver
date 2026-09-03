//! What the navigator sidebar asks a MySQL server about itself, and the connect-
//! time probe that decides which questions it can be asked at all.
//!
//! `information_schema` throughout, with no `SHOW` anywhere, for a reason that
//! is about safety before it is about speed: every `SHOW` variant takes its
//! object name as syntax rather than as a value, so `SHOW TABLE STATUS FROM db
//! LIKE 'name'` forces identifier pasting and a table whose name contains a
//! quote breaks it. All nine calls below bind their schema and relation. That
//! MySQL 8 also made `information_schema` a data-dictionary lookup rather than
//! a temporary table per query is a second reason, not the first one.
//!
//! One rule that is easy to violate by accident: **a dynamic-metadata column
//! never shares a statement with a static one.** `TABLES.TABLE_ROWS` and
//! `STATISTICS.CARDINALITY` are cached under `information_schema_stats_expiry`,
//! and the manual says selecting them alongside static columns adds overhead.
//! That is why `relations` is two statements.
//!
//! Three things the shared structs ask for that MySQL cannot answer, recorded
//! here so nobody goes looking:
//!
//! - `IndexInfo::predicate` is always `None`. MySQL has no partial index.
//! - `TriggerInfo::function` is always `None`. A MySQL trigger has no separate
//!   routine; its body is inline, and that body is carried in `definition`
//!   instead. Putting a `BEGIN … END` block in a field a structure pane renders
//!   as a function name is worse than leaving it empty.
//! - `TriggerInfo::enabled` is always `true`, and that one is a real answer
//!   rather than a gap: MySQL has no way to disable a trigger, so every trigger
//!   in the catalog fires.
//!
//! And several MySQL facts the structs have no room for, the largest being
//! `COLUMNS.EXTRA` — `auto_increment`, `VIRTUAL GENERATED`, `on update
//! CURRENT_TIMESTAMP`. A generated column reported with no default and no
//! marker reads as an ordinary empty column. `STATISTICS.IS_VISIBLE` is the
//! same shape of loss: an invisible index the optimizer will not use, listed as
//! though it were live.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, EndProcess, IndexInfo, InfoField, ProcessInfo,
    RelationInfo, RelationKind, RelationshipInfo, RoutineInfo, RoutineKind, SchemaInfo,
    TriggerInfo, UniqueKeyInfo, VariableInfo, VariableScope,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};
use std::collections::BTreeMap;

use crate::MySqlError;

/// One key part of one index, as `STATISTICS` reports it: name, uniqueness,
/// access method, column, expression, prefix length, sort order.
type StatisticsRow = (
    String,
    i32,
    String,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
);

/// One routine as `information_schema.ROUTINES` reports it: name, type, return
/// type, language, and the argument list assembled from `PARAMETERS`.
type RoutineRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// One column of one foreign key, from whichever side asked: key name, local
/// column, other schema, other table, other column, update rule, delete rule.
///
/// The same tuple for both directions, which is safe only because the two
/// statements below each write their own projection out in full — the sides are
/// swapped in the SQL, not by a flag here.
type RelationshipRow = (String, String, String, String, String, String, String);

/// What this server can do, asked once at connect.
///
/// Probes rather than a version test. TiDB reports a MySQL version that its own
/// `server-version` setting can overwrite, so a version string is not evidence
/// about anything. Two of these are answered by the catalog, because
/// implementing `information_schema` is how these servers announce what they
/// support; the third has no catalog to ask and is answered by trying.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// `information_schema.CHECK_CONSTRAINTS`, which arrived with MySQL 8.0.16
    /// and does not exist on MariaDB before 10.2. `TABLE_CONSTRAINTS.ENFORCED`
    /// arrived with it, so one probe covers both.
    check_constraints: bool,
    /// `STATISTICS.EXPRESSION`, which arrived with MySQL 8.0.13 for functional
    /// key parts and does not exist on MariaDB at all. Selecting a column the
    /// server does not have fails the whole statement, so the column list is
    /// chosen once rather than attempted and retried.
    index_expressions: bool,
    /// Whether the server has the transaction control `TxStep` names. Read by
    /// the driver rather than by this module, which is why it is the one field
    /// here that is not private.
    pub(crate) transactions: bool,
}

pub async fn probe(conn: &mut Conn) -> Result<Capabilities, MySqlError> {
    let check_constraints: Option<u32> = conn
        .query_first(
            "SELECT COUNT(*) FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = 'information_schema' AND TABLE_NAME = 'CHECK_CONSTRAINTS'",
        )
        .await?;
    let index_expressions: Option<u32> = conn
        .query_first(
            "SELECT COUNT(*) FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = 'information_schema' AND TABLE_NAME = 'STATISTICS' \
               AND COLUMN_NAME = 'EXPRESSION'",
        )
        .await?;
    Ok(Capabilities {
        check_constraints: check_constraints.unwrap_or(0) > 0,
        index_expressions: index_expressions.unwrap_or(0) > 0,
        transactions: transactions(conn).await?,
    })
}

/// The savepoint the transaction probe sets and throws away.
const PROBE_SAVEPOINT: &str = "dbclient_probe";

/// Whether this server has the transaction control `TxStep` names, found out by
/// asking it for some.
///
/// The one question here the catalog cannot answer: no `information_schema`
/// table lists the statements a server implements, and the servers this driver
/// reaches do not agree. StarRocks has `BEGIN` and `COMMIT` and stops there —
/// `SAVEPOINT` is a syntax error, and a statement inside an open transaction
/// cannot even read a table that transaction has written. So a driver that
/// concluded "transactional" from `BEGIN` existing would hand a front end a
/// Rollback To button that cannot work and a grid that will not show the row
/// just inserted.
///
/// Two of the six steps are asked and the other four come with them: a server
/// with `BEGIN` has `COMMIT` and `ROLLBACK`, and one with `SAVEPOINT` has the
/// two statements that use a savepoint. That is what all three servers this
/// driver is tested against do; one that had savepoints but no `RELEASE` would
/// refuse that step out loud when it was asked for rather than silently.
///
/// `SAVEPOINT` is asked inside a transaction because that is the only place the
/// answer means anything, and the transaction is rolled back whichever way the
/// answer went — a probe that left one open would put the session's first real
/// statement inside it.
async fn transactions(conn: &mut Conn) -> Result<bool, MySqlError> {
    if !accepted(conn.query_drop("BEGIN").await)? {
        return Ok(false);
    }
    let savepoints = accepted(
        conn.query_drop(format!("SAVEPOINT {PROBE_SAVEPOINT}"))
            .await,
    )?;
    conn.query_drop("ROLLBACK").await?;
    Ok(savepoints)
}

/// A statement the server refused is the answer to the question; a connection
/// that broke is not, and is passed on rather than read as a no.
fn accepted(outcome: Result<(), mysql_async::Error>) -> Result<bool, MySqlError> {
    match outcome {
        Ok(()) => Ok(true),
        Err(mysql_async::Error::Server(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Databases, minus the four the server keeps for itself.
///
/// Filtered here rather than marked and filtered by the caller, which is the
/// PostgreSQL driver's arrangement. `sys` is in the list on our own account
/// rather than upstream's: it is a set of helper views over
/// `performance_schema` and belongs in a sidebar no more than that does.
pub async fn schemas(conn: &mut Conn) -> Result<Vec<SchemaInfo>, MySqlError> {
    // Marked rather than excluded, which is the change the PostgreSQL driver
    // explains: a name left out of the answer is one no setting can put back.
    // The four are still the four; only what is done with them moved.
    //
    // System last, then by name, so a tree that opens on a fresh server does not
    // open on `information_schema`.
    let rows: Vec<(String, i32)> = conn
        .query(
            "SELECT SCHEMA_NAME, \
                    SCHEMA_NAME IN \
                      ('information_schema', 'performance_schema', 'mysql', 'sys') \
             FROM information_schema.SCHEMATA \
             ORDER BY SCHEMA_NAME IN \
                        ('information_schema', 'performance_schema', 'mysql', 'sys'), \
                      SCHEMA_NAME",
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(name, is_system)| SchemaInfo {
            name,
            // MySQL answers a predicate with 1 or 0 rather than with a boolean,
            // and the crate maps that to an integer.
            is_system: is_system != 0,
        })
        .collect())
}

/// `TABLE_TYPE`, and the one kind that is not in it.
///
/// A partitioned table is still `BASE TABLE`; the only thing that says
/// otherwise is the word `partitioned` in `CREATE_OPTIONS`, which is where
/// upstream reads it from too. `CREATE_OPTIONS` is static metadata, so it rides
/// along in the same statement for free.
///
/// An unrecognised `TABLE_TYPE` is `Unknown` rather than an error: StarRocks
/// and MariaDB both have values MySQL does not, and a navigator that refused to
/// list a relation because it had not heard of its kind would be worse than one
/// that showed it without an icon.
fn relation_kind(table_type: &str, create_options: &str) -> RelationKind {
    match table_type {
        "BASE TABLE" | "SYSTEM VERSIONED" => {
            if create_options
                .split_ascii_whitespace()
                .any(|option| option.eq_ignore_ascii_case("partitioned"))
            {
                RelationKind::PartitionedTable
            } else {
                RelationKind::Table
            }
        }
        // MySQL's own `SYSTEM VIEW` covers `information_schema`, which this
        // driver filters out of `schemas` — but a user who names that schema
        // explicitly should still see what is in it.
        "VIEW" | "SYSTEM VIEW" => RelationKind::View,
        _ => RelationKind::Unknown,
    }
}

/// `information_schema.ROUTINES.ROUTINE_TYPE`, as the two-way split the
/// navigator draws.
///
/// Anything but `PROCEDURE` is a function, for the reason the PostgreSQL
/// driver's `routine_kind` gives: of the two wrong guesses, the one that invites
/// a reader to `CALL` something they cannot is the worse.
fn routine_kind(routine_type: &str) -> RoutineKind {
    match routine_type {
        "PROCEDURE" => RoutineKind::Procedure,
        _ => RoutineKind::Function,
    }
}

pub async fn relations(conn: &mut Conn, schema: &str) -> Result<Vec<RelationInfo>, MySqlError> {
    let listed: Vec<(String, String, Option<String>)> = conn
        .exec(
            "SELECT TABLE_NAME, TABLE_TYPE, CREATE_OPTIONS \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? \
             ORDER BY TABLE_NAME",
            (schema,),
        )
        .await?;

    // A second statement, and deliberately. `TABLE_ROWS` is one of the columns
    // `information_schema_stats_expiry` caches, and the manual is explicit that
    // asking for a cached column alongside an uncached one costs more than
    // asking for each. Splitting also lets a caller paint the names before the
    // estimate has arrived, which is the order a sidebar wants them in.
    let counted: Vec<(String, Option<i64>)> = conn
        .exec(
            "SELECT TABLE_NAME, TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
            (schema,),
        )
        .await?;
    let counted: BTreeMap<String, Option<i64>> = counted.into_iter().collect();

    Ok(listed
        .into_iter()
        .map(|(name, table_type, create_options)| RelationInfo {
            kind: relation_kind(&table_type, create_options.as_deref().unwrap_or("")),
            // NULL for a view, and for a table nothing has ever measured.
            // Declining to answer is not the same as answering zero, and the
            // estimate is documented as being off by up to half on InnoDB
            // anyway.
            estimated_rows: counted.get(&name).copied().flatten(),
            schema: schema.to_string(),
            name,
        })
        .collect())
}

/// The functions and procedures in a schema, without their bodies.
///
/// The argument list is assembled from `information_schema.PARAMETERS` rather
/// than read off one column, because MySQL keeps no rendered signature — it
/// keeps a row per parameter. `ORDINAL_POSITION = 0` is a function's *return*
/// and is skipped here; `returns` comes from `DTD_IDENTIFIER` on the routine
/// itself, which holds the same type and is NULL for a procedure.
///
/// `GROUP_CONCAT` has a length ceiling (`group_concat_max_len`, 1024 by
/// default) and a signature long enough to hit it comes back cut off. Left as
/// it is: raising the limit is a session variable this driver would have to set
/// on a pooled connection for every read, and a truncated argument list is a
/// display defect on a routine with dozens of parameters, where the body — one
/// click away — is the real answer.
///
/// One relation's row in `information_schema.TABLES`, as five facts.
type TableInfoRow = (
    Option<String>,
    Option<String>,
    Option<u64>,
    Option<u64>,
    Option<String>,
);

/// What MySQL has to say about one relation.
///
/// The storage engine is the first line and is the one that is genuinely MySQL's
/// own: InnoDB and MyISAM differ in whether there are transactions, whether
/// there are foreign keys, and what a crash costs, and none of that is visible
/// anywhere else in this window.
///
/// The size is computed here rather than by the server, which has no
/// `pg_size_pretty`. Data plus indexes, for the reason the PostgreSQL driver
/// totals the same two.
///
/// Left out where the server answers null, which is what a view answers for
/// every one of these — a view has no engine, no rows and no bytes, and printing
/// zeroes would describe it as an empty table.
pub async fn table_info(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
) -> Result<Vec<InfoField>, MySqlError> {
    let rows: Vec<TableInfoRow> = conn
        .exec(
            "SELECT ENGINE, TABLE_COLLATION, DATA_LENGTH, INDEX_LENGTH, TABLE_COMMENT \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            (schema, relation),
        )
        .await?;
    let Some((engine, collation, data, index, comment)) = rows.into_iter().next() else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    if let Some(engine) = engine {
        out.push(InfoField {
            label: "Storage engine".to_string(),
            value: engine,
        });
    }
    if let Some(collation) = collation {
        out.push(InfoField {
            label: "Collation".to_string(),
            value: collation,
        });
    }
    // Either half may be null on its own — a table with no index has no index
    // length — so the two are added as zero-if-absent and the row is drawn only
    // if the server gave at least one of them.
    if data.is_some() || index.is_some() {
        out.push(InfoField {
            label: "Size".to_string(),
            value: size_pretty(data.unwrap_or(0) + index.unwrap_or(0)),
        });
    }
    // MySQL writes an empty string rather than null for a table nobody
    // commented, so emptiness is what is tested and not nullness.
    if let Some(comment) = comment.filter(|c| !c.is_empty()) {
        out.push(InfoField {
            label: "Comment".to_string(),
            value: comment,
        });
    }
    Ok(out)
}

/// Bytes as somebody reads them.
///
/// Powers of 1024 with the units the server's own documentation uses, and one
/// decimal place above a kilobyte: "45.3 MB" is the number somebody wanted and
/// "47,513,600 bytes" is a number they have to count the digits of.
fn size_pretty(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// `ROUTINE_BODY` is the language, and it is `SQL` for everything MySQL can
/// create. It is read anyway rather than hard-coded: MariaDB and MySQL 8 both
/// list the column, and a server that one day answers something else says so
/// here rather than being described by a constant in this file.
pub async fn routines(conn: &mut Conn, schema: &str) -> Result<Vec<RoutineInfo>, MySqlError> {
    let rows: Vec<RoutineRow> = conn
        .exec(
            "SELECT r.ROUTINE_NAME, r.ROUTINE_TYPE, r.DTD_IDENTIFIER, r.ROUTINE_BODY, \
                    (SELECT GROUP_CONCAT(p.DTD_IDENTIFIER ORDER BY p.ORDINAL_POSITION \
                                         SEPARATOR ', ') \
                       FROM information_schema.PARAMETERS p \
                      WHERE p.SPECIFIC_SCHEMA = r.ROUTINE_SCHEMA \
                        AND p.SPECIFIC_NAME = r.ROUTINE_NAME \
                        AND p.ROUTINE_TYPE = r.ROUTINE_TYPE \
                        AND p.ORDINAL_POSITION > 0) \
             FROM information_schema.ROUTINES r \
             WHERE r.ROUTINE_SCHEMA = ? \
             ORDER BY r.ROUTINE_NAME, r.ROUTINE_TYPE",
            (schema,),
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(
            |(name, routine_type, returns, body, arguments)| RoutineInfo {
                schema: schema.to_string(),
                kind: routine_kind(&routine_type),
                // Type and name together, because a schema may hold a FUNCTION and
                // a PROCEDURE of the same name — MySQL keeps them in separate
                // namespaces — and the name alone would address both.
                id: format!("{routine_type} {name}"),
                name,
                arguments: arguments.unwrap_or_default(),
                returns,
                language: body,
            },
        )
        .collect())
}

/// One routine's source, by the `TYPE name` id `routines` reported.
///
/// `ROUTINE_DEFINITION` is the body only — MySQL keeps no `CREATE` header — and
/// it is NULL for a caller without `SELECT` on `mysql.proc` or the routine's
/// definer privileges. NULL comes back as `None`, which is the honest answer:
/// the routine is there and this connection may not read it.
pub async fn routine_definition(
    conn: &mut Conn,
    schema: &str,
    id: &str,
) -> Result<Option<String>, MySqlError> {
    let rows: Vec<(Option<String>,)> = conn
        .exec(
            "SELECT ROUTINE_DEFINITION FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND CONCAT(ROUTINE_TYPE, ' ', ROUTINE_NAME) = ?",
            (schema, id),
        )
        .await?;
    Ok(rows.into_iter().next().and_then(|(body,)| body))
}

pub async fn columns(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
) -> Result<Vec<ColumnInfo>, MySqlError> {
    // COLUMN_TYPE rather than DATA_TYPE: the second is the bare name
    // (`decimal`), the first is the full rendering (`decimal(18,4)`,
    // `int unsigned`, `enum('a','b')`) — which is what the field asks for and
    // what `format_type` gives the PostgreSQL driver.
    let rows: Vec<(String, String, String, u32, String, Option<String>)> = conn
        .exec(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, ORDINAL_POSITION, \
                    COLUMN_KEY, COLUMN_DEFAULT \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY ORDINAL_POSITION",
            (schema, relation),
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(
            |(name, data_type, nullable, position, key, default_value)| ColumnInfo {
                name,
                data_type,
                nullable: nullable == "YES",
                // Already one-based, so unlike SQLite's `cid` there is nothing
                // to fix up here.
                position: position as i32,
                // `COLUMN_KEY` holds one value with a documented priority —
                // `PRI` over `UNI` over `MUL` — so a column in both the primary
                // key and another index still reports `PRI`. An unrecognised
                // value means "not a primary key" rather than an error;
                // StarRocks spells the third one `DUP`.
                is_primary_key: key == "PRI",
                // The server's own text, verbatim. Upstream re-quotes it
                // because it is assembling DDL; this is reporting what the
                // catalog holds. The ambiguity that leaves is real and is in
                // `EXTRA`, which has nowhere to go: a MySQL 8 expression
                // default is stored unquoted, so `CURRENT_TIMESTAMP` and the
                // literal string of the same name look alike from here.
                default_value,
                // `COLUMN_DEFAULT` is NULL for a generated column — MySQL keeps
                // that expression in `GENERATION_EXPRESSION`, which this query
                // does not ask for — so nothing here can be a computation.
                computed: None,
            },
        )
        .collect())
}

/// The body of a view.
///
/// `None` when there is no row, which is exactly "this relation is not a view" —
/// MySQL has no materialized view, so unlike PostgreSQL there is no kind filter
/// to write. The row's existence is the filter.
///
/// The body only, without the `CREATE VIEW` around it, which matches
/// PostgreSQL's `pg_get_viewdef` and differs from SQLite, where the stored text
/// is the whole statement.
pub async fn definition(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
) -> Result<Option<String>, MySqlError> {
    let found: Option<String> = conn
        .exec_first(
            "SELECT VIEW_DEFINITION FROM information_schema.VIEWS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            (schema, relation),
        )
        .await?;
    Ok(found)
}

/// One key part as it should be printed.
///
/// An index on `email(10)` is not an index on `email`, and one on `qty DESC` is
/// not one on `qty` — printing either without its marker misstates what the
/// planner can use. This is the same job PostgreSQL's `pg_get_indexdef` does per
/// position, done here because MySQL has no equivalent function.
fn key_part(
    column: Option<String>,
    expression: Option<String>,
    prefix: Option<u32>,
    collation: Option<String>,
) -> String {
    // A functional key part reports a NULL column and an expression; an ordinary
    // one reports the reverse.
    let mut part = match (expression, column) {
        (Some(expression), _) => format!("({expression})"),
        (None, Some(column)) => match prefix {
            Some(prefix) => format!("`{column}`({prefix})"),
            None => format!("`{column}`"),
        },
        (None, None) => return String::new(),
    };
    // 'A' is ascending, 'D' descending, NULL not sorted. Only the second is
    // worth printing; the other two are the default and noise.
    if collation.as_deref() == Some("D") {
        part.push_str(" DESC");
    }
    part
}

/// The column list for `STATISTICS`, which is not the same on every server.
///
/// `EXPRESSION` does not exist before MySQL 8.0.13 and does not exist on
/// MariaDB, and selecting a column the server has never heard of fails the
/// whole statement rather than the one column.
fn statistics_columns(caps: &Capabilities) -> &'static str {
    if caps.index_expressions {
        "INDEX_NAME, NON_UNIQUE, INDEX_TYPE, COLUMN_NAME, EXPRESSION, SUB_PART, COLLATION"
    } else {
        "INDEX_NAME, NON_UNIQUE, INDEX_TYPE, COLUMN_NAME, NULL, SUB_PART, COLLATION"
    }
}

/// Every key part of every index, in index order, primary key first.
///
/// Read once and shaped by the two callers rather than twice: `indexes` wants
/// the part as the text a structure pane prints, `unique_keys` wants the column
/// name on its own, and the rows they want are the same rows.
async fn statistics(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
    caps: &Capabilities,
) -> Result<Vec<StatisticsRow>, MySqlError> {
    // `CARDINALITY` is deliberately absent: it is cached under
    // `information_schema_stats_expiry` and `IndexInfo` has no field for it.
    let sql = format!(
        "SELECT {} FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
         ORDER BY (INDEX_NAME = 'PRIMARY') DESC, INDEX_NAME, SEQ_IN_INDEX",
        statistics_columns(caps)
    );
    Ok(conn.exec(sql, (schema, relation)).await?)
}

/// Key parts per index, in index order, primary key first.
async fn key_parts(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
    caps: &Capabilities,
) -> Result<Vec<(String, bool, String, String)>, MySqlError> {
    Ok(statistics(conn, schema, relation, caps)
        .await?
        .into_iter()
        .map(
            |(name, non_unique, method, column, expression, prefix, collation)| {
                (
                    name,
                    non_unique == 0,
                    method,
                    key_part(column, expression, prefix, collation),
                )
            },
        )
        .collect())
}

pub async fn indexes(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
    caps: &Capabilities,
) -> Result<Vec<IndexInfo>, MySqlError> {
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for (name, is_unique, method, part) in key_parts(conn, schema, relation, caps).await? {
        // One row per key part, already grouped by the ORDER BY, so the last
        // index built is the one this row belongs to.
        if indexes.last().is_some_and(|last| last.name == name) {
            indexes.last_mut().expect("just checked").columns.push(part);
            continue;
        }
        indexes.push(IndexInfo {
            // Upstream's own test, and there is no better one: MySQL's primary
            // key is the index literally named PRIMARY, and no other index may
            // take that name.
            is_primary: name == "PRIMARY",
            name,
            columns: vec![part],
            is_unique,
            method,
            // MySQL has no partial index, so there is never a predicate. See
            // the module comment.
            predicate: None,
        });
    }
    Ok(indexes)
}

/// UNIQUE keys that name columns, primary key excluded.
///
/// The same `STATISTICS` rows `indexes` reads, filtered to `NON_UNIQUE = 0` and
/// gathered into whole keys — MySQL has no separate notion of a unique
/// constraint, `UNIQUE KEY` is an index and `information_schema` reports it as
/// one.
///
/// A functional key part — `UNIQUE ((price * qty))`, where `COLUMN_NAME` is NULL
/// and `EXPRESSION` holds the text — disqualifies the whole key, because a key
/// this cannot state as columns is one no `WHERE` clause can reproduce.
///
/// A prefix part — `UNIQUE (name(10))` — is kept, and the column is named
/// without the prefix. Uniqueness over the first ten characters is uniqueness
/// over the whole value: two rows that share the value would share the prefix,
/// and the server would already have refused the second one.
pub async fn unique_keys(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
    caps: &Capabilities,
) -> Result<Vec<UniqueKeyInfo>, MySqlError> {
    let mut keys: Vec<UniqueKeyInfo> = Vec::new();
    let mut functional: Vec<String> = Vec::new();
    for (name, non_unique, _, column, expression, _, _) in
        statistics(conn, schema, relation, caps).await?
    {
        // Upstream's own test for the primary key, and there is no better one:
        // it is the index literally named PRIMARY, and no other index may take
        // that name. It is left out because `ColumnInfo::is_primary_key`
        // already carries it.
        if non_unique != 0 || name == "PRIMARY" {
            continue;
        }
        let Some(column) = column.filter(|_| expression.is_none()) else {
            // Remembered rather than acted on here: the parts of a key arrive
            // one row at a time, and the functional one may not be the first.
            functional.push(name);
            continue;
        };
        // One row per key part, already grouped by the ORDER BY, so the last
        // key built is the one this row belongs to.
        match keys.last_mut() {
            Some(last) if last.name == name => last.columns.push(column),
            _ => keys.push(UniqueKeyInfo {
                name,
                columns: vec![column],
            }),
        }
    }
    keys.retain(|key| !functional.contains(&key.name));
    Ok(keys)
}

/// Foreign keys this relation declares.
pub async fn foreign_keys(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, MySqlError> {
    // Two statements with the sides written out rather than one parameterised
    // by direction, for the reason the PostgreSQL driver gives: the sides swap
    // in three places, and a query that decides which array is "local" from a
    // flag is one edit away from reporting a key backwards.
    //
    // `kcu.TABLE_NAME = rc.TABLE_NAME` is in the join and upstream's is not.
    // `KEY_COLUMN_USAGE` also holds rows for primary and unique keys, so
    // without it a foreign key sharing a name with a key on another table in
    // the same schema joins to the wrong rows.
    let rows: Vec<RelationshipRow> = conn
        .exec(
            "SELECT rc.CONSTRAINT_NAME, kcu.COLUMN_NAME, \
                    kcu.REFERENCED_TABLE_SCHEMA, kcu.REFERENCED_TABLE_NAME, \
                    kcu.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
             FROM information_schema.REFERENTIAL_CONSTRAINTS rc \
             JOIN information_schema.KEY_COLUMN_USAGE kcu \
               ON  kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA \
               AND kcu.CONSTRAINT_NAME   = rc.CONSTRAINT_NAME \
               AND kcu.TABLE_NAME        = rc.TABLE_NAME \
             WHERE rc.CONSTRAINT_SCHEMA = ? AND rc.TABLE_NAME = ? \
             ORDER BY rc.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            (schema, relation),
        )
        .await?;

    Ok(gather(rows.into_iter()))
}

/// Foreign keys other relations declare against this one.
pub async fn referenced_by(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, MySqlError> {
    // The referencing side may live in another database — a cross-database
    // foreign key is ordinary in MySQL and impossible in PostgreSQL — so the
    // projection reads `kcu.CONSTRAINT_SCHEMA` rather than assuming the schema
    // that was asked about.
    let rows: Vec<RelationshipRow> = conn
        .exec(
            "SELECT rc.CONSTRAINT_NAME, kcu.REFERENCED_COLUMN_NAME, \
                    kcu.CONSTRAINT_SCHEMA, rc.TABLE_NAME, kcu.COLUMN_NAME, \
                    rc.UPDATE_RULE, rc.DELETE_RULE \
             FROM information_schema.REFERENTIAL_CONSTRAINTS rc \
             JOIN information_schema.KEY_COLUMN_USAGE kcu \
               ON  kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA \
               AND kcu.CONSTRAINT_NAME   = rc.CONSTRAINT_NAME \
               AND kcu.TABLE_NAME        = rc.TABLE_NAME \
             WHERE rc.UNIQUE_CONSTRAINT_SCHEMA = ? AND rc.REFERENCED_TABLE_NAME = ? \
             ORDER BY kcu.CONSTRAINT_SCHEMA, rc.TABLE_NAME, rc.CONSTRAINT_NAME, \
                      kcu.ORDINAL_POSITION",
            (schema, relation),
        )
        .await?;

    Ok(gather(rows.into_iter()))
}

/// Collects one row per key column into one entry per key.
///
/// The two column arrays line up for free, which they do not in PostgreSQL:
/// `KEY_COLUMN_USAGE` carries the local column and the one it references in the
/// same row, so ordering by `ORDINAL_POSITION` builds both sides in agreement.
///
/// `on_update` and `on_delete` are used verbatim. `UPDATE_RULE` and
/// `DELETE_RULE` are already spelled the way the DDL spells them — `CASCADE`,
/// `SET NULL`, `RESTRICT`, `NO ACTION` — so there is no translation to write.
fn gather<I: Iterator<Item = RelationshipRow>>(rows: I) -> Vec<RelationshipInfo> {
    let mut out: Vec<RelationshipInfo> = Vec::new();
    for (name, local, other_schema, other_table, other, on_update, on_delete) in rows {
        if let Some(last) = out.last_mut()
            && last.name == name
            && last.other_table == other_table
            && last.other_schema == other_schema
        {
            last.local_columns.push(local);
            last.other_columns.push(other);
            continue;
        }
        out.push(RelationshipInfo {
            name,
            local_columns: vec![local],
            other_schema,
            other_table,
            other_columns: vec![other],
            on_update,
            on_delete,
        });
    }
    out
}

/// The statement `constraints` runs, which depends on what the catalog has.
///
/// `CHECK_CONSTRAINTS` has no `TABLE_NAME` column on MySQL — it holds only the
/// catalog, schema, name and clause — so there is no way to learn which table a
/// check belongs to without joining `TABLE_CONSTRAINTS`. The join is therefore
/// not an optimisation, it is the only way to ask the question. On a server
/// without the table, unique constraints still work.
fn constraints_sql(caps: &Capabilities) -> &'static str {
    if caps.check_constraints {
        "SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, tc.ENFORCED, cc.CHECK_CLAUSE \
         FROM information_schema.TABLE_CONSTRAINTS tc \
         LEFT JOIN information_schema.CHECK_CONSTRAINTS cc \
           ON  cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
           AND cc.CONSTRAINT_NAME   = tc.CONSTRAINT_NAME \
         WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? \
           AND tc.CONSTRAINT_TYPE IN ('UNIQUE', 'CHECK') \
         ORDER BY tc.CONSTRAINT_TYPE, tc.CONSTRAINT_NAME"
    } else {
        // `ENFORCED` arrived with CHECK support, so a server without one has
        // neither.
        "SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, 'YES', NULL \
         FROM information_schema.TABLE_CONSTRAINTS tc \
         WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? \
           AND tc.CONSTRAINT_TYPE = 'UNIQUE' \
         ORDER BY tc.CONSTRAINT_NAME"
    }
}

/// CHECK and UNIQUE constraints.
///
/// Primary and foreign keys are left out because each already has its own
/// section, and listing a key twice invites the reader to wonder whether they
/// are two different things. `EXCLUDE` has no MySQL analogue and never appears.
pub async fn constraints(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
    caps: &Capabilities,
) -> Result<Vec<ConstraintInfo>, MySqlError> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> =
        conn.exec(constraints_sql(caps), (schema, relation)).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // A unique constraint is backed by an index of the same name, and its
    // columns are only in STATISTICS. Fetched once for the whole relation
    // rather than per constraint, and only when something needs it.
    let wants_columns = rows.iter().any(|(_, kind, _, _)| kind == "UNIQUE");
    let mut unique_columns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if wants_columns {
        for (name, _, _, part) in key_parts(conn, schema, relation, caps).await? {
            unique_columns.entry(name).or_default().push(part);
        }
    }

    Ok(rows
        .into_iter()
        .map(|(name, kind, enforced, clause)| {
            // MySQL has no `pg_get_constraintdef`, so the rendering is built
            // here — but only the keyword and the parentheses are constructed.
            // `CHECK_CLAUSE` is already the server's own rendering of the
            // expression, which is the part that would be got subtly wrong by
            // reassembling it.
            let mut definition = match kind.as_str() {
                "CHECK" => format!("CHECK ({})", clause.unwrap_or_default()),
                "UNIQUE" => format!(
                    "UNIQUE ({})",
                    unique_columns
                        .get(&name)
                        .cloned()
                        .unwrap_or_default()
                        .join(", ")
                ),
                other => other.to_string(),
            };
            // A check the server does not enforce, listed as though it does, is
            // the disabled-trigger lie in another place. It goes in the
            // definition because that field is already "how the server states
            // it", and widening the shared struct for one driver's one flag
            // would be the wrong trade.
            if enforced.as_deref() == Some("NO") {
                definition.push_str(" /* NOT ENFORCED */");
            }
            ConstraintInfo {
                name,
                kind: match kind.as_str() {
                    "CHECK" => ConstraintKind::Check,
                    "UNIQUE" => ConstraintKind::Unique,
                    _ => ConstraintKind::Other,
                },
                definition,
            }
        })
        .collect())
}

pub async fn triggers(
    conn: &mut Conn,
    schema: &str,
    relation: &str,
) -> Result<Vec<TriggerInfo>, MySqlError> {
    // Filtered on the table the trigger fires for rather than on the schema the
    // trigger lives in. Upstream loads a whole schema and matches client-side;
    // a per-relation call wants the server to do it.
    let rows: Vec<(String, String, String, String, Option<String>)> = conn
        .exec(
            "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, \
                    ACTION_ORIENTATION, ACTION_STATEMENT \
             FROM information_schema.TRIGGERS \
             WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? \
             ORDER BY ACTION_TIMING, EVENT_MANIPULATION, ACTION_ORDER, TRIGGER_NAME",
            (schema, relation),
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|(name, timing, event, level, statement)| TriggerInfo {
            name,
            // BEFORE or AFTER. MySQL has no INSTEAD OF.
            timing: Some(timing),
            // Always exactly one. MySQL has no `BEFORE INSERT OR UPDATE`
            // trigger and no TRUNCATE trigger, so where PostgreSQL reports a
            // set this reports a single element — which is the honest shape,
            // not a degenerate one.
            events: vec![event],
            // Documented as always ROW, and read rather than assumed so that a
            // server which ever says otherwise is believed.
            level: Some(level),
            // See the module comment: there is no separate routine to name.
            function: None,
            // MySQL has no disabled trigger; everything in the catalog fires.
            enabled: true,
            definition: statement,
        })
        .collect())
}

/// Everything `SHOW FULL PROCESSLIST` reports.
///
/// `SHOW` rather than `information_schema.PROCESSLIST`, which would have been
/// the tidier read — a typed tuple, `coalesce`, `SEC_TO_TIME` doing the
/// formatting server-side. This driver reaches MariaDB, StarRocks and Doris as
/// well as MySQL, and the `SHOW` form is the one all four have had since before
/// any of them forked; the table is a convenience some of them added later.
/// Nothing else in this file has to make that choice, because nothing else in
/// this file is asking about the server rather than about the catalog.
///
/// Read by position and not by arity: MariaDB appends a `Progress` column, so a
/// tuple of eight would fail to decode there while the first eight columns are
/// the same eight everywhere.
///
/// "Everywhere" is not every server that speaks this protocol, which is why the
/// reads below go through `get_opt` rather than `get`. StarRocks answers this
/// statement with a shape of its own — a `ServerName` where the id goes, and
/// four more columns after `Info` — and `get` on a value it cannot convert
/// panics, which in a driver means taking the application down over a server
/// that answered the question differently. The failure it reports instead names
/// the column, which is the sentence somebody can act on.
///
/// The duration is formatted here rather than by the server, which is the one
/// place this file departs from the rule `table_info` follows. `SHOW` takes no
/// expressions, so `SEC_TO_TIME` is not available — and seconds into hours,
/// minutes and seconds is arithmetic with no units question and no rounding to
/// disagree about, which is what made the rule worth following elsewhere.
pub async fn processes(conn: &mut Conn) -> Result<Vec<ProcessInfo>, MySqlError> {
    let rows: Vec<Row> = conn.query("SHOW FULL PROCESSLIST").await?;
    rows.iter().map(one_process).collect()
}

/// One row of `SHOW FULL PROCESSLIST`, in the shape MySQL answers it.
fn one_process(row: &Row) -> Result<ProcessInfo, MySqlError> {
    let text = |index: usize| -> Result<Option<String>, MySqlError> {
        Ok(column::<String>(row, index, "a string")?.flatten())
    };
    Ok(ProcessInfo {
        id: column::<u64>(row, 0, "a thread id")?
            .flatten()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        user: text(1)?.unwrap_or_default(),
        database: text(3)?.unwrap_or_default(),
        // Two columns, because MySQL splits what one question means. `Command`
        // is what the connection is doing at all — `Sleep`, `Query`, `Binlog
        // Dump` — and `State` is where inside a statement it has got to:
        // `Sending data`, `Waiting for table metadata lock`. The second is the
        // one that says why a query is not finishing, and dropping it would
        // leave every busy connection reading `Query`.
        state: match text(6)?.filter(|state| !state.is_empty()) {
            Some(detail) => format!("{} · {detail}", text(4)?.unwrap_or_default()),
            None => text(4)?.unwrap_or_default(),
        },
        duration: column::<i64>(row, 5, "a number of seconds")?
            .flatten()
            .map(clock)
            .unwrap_or_default(),
        statement: text(7)?.unwrap_or_default(),
    })
}

/// One column of one row, or the reason it is not what was asked for.
///
/// The outer `Option` is "there is no such column", which the callers above read
/// as an empty field: a server with fewer columns has told us what it has. The
/// inner one is SQL NULL. A value of the wrong type is neither, and is the
/// failure this exists to return instead of panicking.
fn column<T>(
    row: &Row,
    index: usize,
    expected: &'static str,
) -> Result<Option<Option<T>>, MySqlError>
where
    T: mysql_async::prelude::FromValue,
{
    match row.get_opt::<Option<T>, usize>(index) {
        None => Ok(None),
        Some(Ok(value)) => Ok(Some(value)),
        Some(Err(e)) => Err(MySqlError::Decode {
            column: row
                .columns_ref()
                .get(index)
                .map(|c| c.name_str().to_string())
                .unwrap_or_else(|| index.to_string()),
            expected,
            value: format!("{:?}", e.0),
        }),
    }
}

/// Seconds as `H:MM:SS`, the shape `SEC_TO_TIME` would have returned.
///
/// Signed because `Time` is: a replica applying a transaction committed in the
/// future on the source reports a negative one, and printing it as an enormous
/// positive number would be worse than printing the minus sign.
fn clock(seconds: i64) -> String {
    let sign = if seconds < 0 { "-" } else { "" };
    let seconds = seconds.unsigned_abs();
    format!(
        "{sign}{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

/// `KILL QUERY <id>` or `KILL <id>`, by the id `processes` gave.
///
/// A thread that is not there comes back `false` rather than as a failure, for
/// the reason the trait gives: a connection that ended between the list being
/// drawn and a row being chosen has already done what was asked of it. MySQL
/// says so with error 1094, which is the only one folded away here — 1095, the
/// refusal to kill somebody else's thread, is a real answer and is passed on.
pub async fn end_process(conn: &mut Conn, id: &str, how: EndProcess) -> Result<bool, MySqlError> {
    // The id came from `processes` and is a thread id, so anything else is a
    // caller that made one up. Refused by name rather than interpolated, which
    // is also what keeps this `format!` out of reach of anything but digits.
    let thread: u64 = id
        .parse()
        .map_err(|_| MySqlError::NotAThread(id.to_string()))?;
    let statement = match how {
        EndProcess::Statement => format!("KILL QUERY {thread}"),
        EndProcess::Session => format!("KILL {thread}"),
    };
    match conn.query_drop(statement).await {
        Ok(()) => Ok(true),
        Err(mysql_async::Error::Server(e)) if e.code == 1094 => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Every setting, from the two `SHOW VARIABLES` this server answers.
///
/// Two statements because MySQL will not say in one which scope a value belongs
/// to. `SHOW SESSION VARIABLES` is the wider list and the one to walk: it names
/// every setting this connection has, which is the server's own plus the two
/// dozen that exist only per-connection — `timestamp`, `rand_seed1`,
/// `pseudo_thread_id`. `SHOW GLOBAL VARIABLES` names only the server's.
///
/// A name in both is `Server` when the two values agree and `Session` when they
/// do not, because a session value that differs from the global one differs for
/// exactly one reason: something set it on this connection. That is the same
/// fact PostgreSQL reports in `pg_settings.source`, arrived at the only way this
/// server offers.
///
/// Not `performance_schema.variables_info`, which has a `VARIABLE_SOURCE` column
/// saying where each value came from and would have answered this in one query.
/// It is empty on a server started with `performance_schema=OFF`, and empty
/// there is indistinguishable from a server with no settings — the sheet would
/// say "no variables" about a server with six hundred. `SHOW VARIABLES` is
/// answered by every MySQL there has ever been.
pub async fn variables(conn: &mut Conn) -> Result<Vec<VariableInfo>, MySqlError> {
    let read = |rows: Vec<Row>| -> BTreeMap<String, String> {
        rows.iter()
            .filter_map(|row| {
                let text = |index: usize| row.get::<Option<String>, usize>(index).flatten();
                // A row with no name is not a setting. MySQL does not send one;
                // dropping it here is what keeps a malformed answer out of a
                // list the front end keys by name.
                Some((text(0)?, text(1).unwrap_or_default()))
            })
            .collect()
    };
    let server = read(conn.query("SHOW GLOBAL VARIABLES").await?);
    let session = read(conn.query("SHOW SESSION VARIABLES").await?);

    // `BTreeMap` and not the order the server sent, which is the ordering by
    // name the trait asks for and is also what makes the two lists comparable.
    Ok(session
        .into_iter()
        .map(|(name, value)| {
            let scope = match server.get(&name) {
                Some(global) if *global == value => VariableScope::Server,
                _ => VariableScope::Session,
            };
            VariableInfo { name, value, scope }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the two the statements below choose their columns by; nothing here
    /// reads a transaction, so it is fixed rather than made an argument nobody
    /// would vary.
    fn caps(check_constraints: bool, index_expressions: bool) -> Capabilities {
        Capabilities {
            check_constraints,
            index_expressions,
            transactions: true,
        }
    }

    #[test]
    fn a_partitioned_table_is_only_visible_in_create_options() {
        // `TABLE_TYPE` says `BASE TABLE` for both, so the word in
        // `CREATE_OPTIONS` is the whole difference.
        assert_eq!(
            relation_kind("BASE TABLE", "partitioned"),
            RelationKind::PartitionedTable
        );
        assert_eq!(relation_kind("BASE TABLE", ""), RelationKind::Table);
        // Other options keep their company, and a substring match on a name
        // like `row_format=partitioned_something` should not fire.
        assert_eq!(
            relation_kind("BASE TABLE", "row_format=DYNAMIC partitioned"),
            RelationKind::PartitionedTable
        );
        assert_eq!(
            relation_kind("BASE TABLE", "stats_persistent=1"),
            RelationKind::Table
        );
    }

    #[test]
    fn an_unheard_of_table_type_is_listed_rather_than_refused() {
        // MariaDB has SEQUENCE and StarRocks has kinds of its own. A navigator
        // that failed on one would hide every relation in the schema.
        assert_eq!(relation_kind("SEQUENCE", ""), RelationKind::Unknown);
        assert_eq!(relation_kind("VIEW", ""), RelationKind::View);
        assert_eq!(relation_kind("SYSTEM VIEW", ""), RelationKind::View);
    }

    #[test]
    fn a_key_part_says_what_the_planner_can_actually_use() {
        assert_eq!(key_part(Some("email".into()), None, None, None), "`email`");
        // A prefix index on the first 16 characters is not an index on the
        // column.
        assert_eq!(
            key_part(Some("email".into()), None, Some(16), Some("A".into())),
            "`email`(16)"
        );
        assert_eq!(
            key_part(Some("qty".into()), None, None, Some("D".into())),
            "`qty` DESC"
        );
        // A functional key part reports a NULL column and the expression
        // instead.
        assert_eq!(
            key_part(None, Some("lower(`email`)".into()), None, Some("A".into())),
            "(lower(`email`))"
        );
    }

    #[test]
    fn the_statistics_column_list_follows_the_catalog_and_not_a_version() {
        // Asking for EXPRESSION on a server that has never had it fails the
        // whole statement, so the list is chosen once from what the catalog
        // reported rather than attempted and retried.
        assert!(statistics_columns(&caps(true, true)).contains("EXPRESSION"));
        assert!(!statistics_columns(&caps(true, false)).contains("EXPRESSION"));
        // The shape has to stay the same either way, because one decoder reads
        // both.
        assert_eq!(
            statistics_columns(&caps(true, true)).split(',').count(),
            statistics_columns(&caps(true, false)).split(',').count()
        );
    }

    #[test]
    fn without_check_constraints_the_join_goes_and_unique_stays() {
        let with = constraints_sql(&caps(true, true));
        assert!(with.contains("CHECK_CONSTRAINTS"));
        assert!(with.contains("'UNIQUE', 'CHECK'"));

        // TiDB with `tidb_enable_check_constraint` off, MariaDB before 10.2,
        // and StarRocks all land here. Unique constraints are still real.
        let without = constraints_sql(&caps(false, false));
        assert!(!without.contains("CHECK_CONSTRAINTS"));
        assert!(!without.contains("ENFORCED"));
        assert!(without.contains("'UNIQUE'"));
    }

    #[test]
    fn a_composite_key_keeps_its_two_sides_in_step() {
        // Both columns come off the same row, so the pairing cannot drift —
        // which is the thing PostgreSQL needs WITH ORDINALITY on both arrays to
        // guarantee.
        let rows = vec![
            (
                "fk".to_string(),
                "a".to_string(),
                "other_db".to_string(),
                "parent".to_string(),
                "x".to_string(),
                "RESTRICT".to_string(),
                "CASCADE".to_string(),
            ),
            (
                "fk".to_string(),
                "b".to_string(),
                "other_db".to_string(),
                "parent".to_string(),
                "y".to_string(),
                "RESTRICT".to_string(),
                "CASCADE".to_string(),
            ),
        ];
        let gathered = gather(rows.into_iter());
        assert_eq!(gathered.len(), 1);
        assert_eq!(gathered[0].local_columns, ["a", "b"]);
        assert_eq!(gathered[0].other_columns, ["x", "y"]);
        // A cross-database reference, which MySQL has and PostgreSQL cannot.
        assert_eq!(gathered[0].other_schema, "other_db");
    }

    #[test]
    fn two_keys_of_the_same_name_on_different_tables_stay_apart() {
        // Inbound references are grouped by name, and two tables may each
        // declare a key called `parent_fk`. Grouping on the name alone would
        // merge them into one relationship pointing at the wrong table.
        let row = |table: &str, column: &str| {
            (
                "parent_fk".to_string(),
                "id".to_string(),
                "app".to_string(),
                table.to_string(),
                column.to_string(),
                "NO ACTION".to_string(),
                "CASCADE".to_string(),
            )
        };
        let gathered = gather(vec![row("orders", "a"), row("invoices", "b")].into_iter());
        assert_eq!(gathered.len(), 2);
        assert_eq!(gathered[0].other_table, "orders");
        assert_eq!(gathered[1].other_table, "invoices");
    }
}
