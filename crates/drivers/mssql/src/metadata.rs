//! Schema introspection for the navigator sidebar.
//!
//! Reads `sys.*` and not `INFORMATION_SCHEMA`, for reasons stronger than the
//! PostgreSQL driver's preference for `pg_catalog`. `INFORMATION_SCHEMA` cannot
//! express this database: it has no view for an index's included columns, a
//! filtered index's predicate, a clustered or columnstore index kind, a computed
//! column, an identity column, or a disabled trigger — and this file reads every
//! one of those. It also has no object ids, so its joins are over three-part
//! names, which is both slower and wrong across a case-insensitive collation.
//!
//! Everything here is read in the one database the connection named. See the
//! crate doc for why that is the whole of what a connection can see.
//!
//! Unlike result data, metadata is small and crosses the FFI as JSON. Arrow buys
//! nothing for a few thousand short rows.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo, UniqueKeyInfo,
};
use tiberius::Row;

use crate::{DatabaseInfo, MsSqlError, Tds};

/// Runs a catalog query to completion and hands back its rows.
///
/// To completion, and that matters: a connection whose stream was abandoned
/// part-read is one tiberius will flush before its next use, and these
/// connections go back into a pool for somebody else.
async fn rows(
    client: &mut Tds,
    sql: &str,
    params: &[&dyn tiberius::ToSql],
) -> Result<Vec<Row>, MsSqlError> {
    Ok(client.query(sql, params).await?.into_first_result().await?)
}

/// The eleven fixed database-role schemas plus the two system ones.
///
/// Named explicitly rather than hidden by the trick upstream uses, which is to
/// join `sys.schemas` against `sys.all_objects` and so show only schemas that
/// contain something. That does hide these without listing them, but it also
/// hides a user schema somebody has just created — it is in the tree one moment
/// and gone the next — and an empty schema is a real thing to want to see.
const HIDDEN_SCHEMAS: &str = "'sys','INFORMATION_SCHEMA','guest','db_owner',\
     'db_accessadmin','db_securityadmin','db_ddladmin','db_backupoperator',\
     'db_datareader','db_datawriter','db_denydatareader','db_denydatawriter'";

/// `sys.objects.type`, as one of the kinds the navigator knows about.
///
/// No materialized view: SQL Server's equivalent is an *indexed* view, which is
/// still `type = 'V'` and is told apart only by looking for a clustered index on
/// it. Reporting one as a plain view is a smaller lie than reporting every view
/// as materialized.
fn relation_kind(object_type: &str, is_external: bool) -> RelationKind {
    if is_external {
        return RelationKind::ForeignTable;
    }
    match object_type.trim() {
        "U" => RelationKind::Table,
        "V" => RelationKind::View,
        _ => RelationKind::Unknown,
    }
}

/// `sys.foreign_keys.update_referential_action`, spelled the way the DDL spells
/// it.
fn referential_action(code: u8) -> String {
    match code {
        1 => "CASCADE",
        2 => "SET NULL",
        3 => "SET DEFAULT",
        // 0 is the default, and anything unrecognised is safest read as the
        // action that does nothing.
        _ => "NO ACTION",
    }
    .to_string()
}

/// The type as the database states it, rebuilt from the catalog.
///
/// There is no `format_type()` to call here, so the rendering is this side's
/// job. Two catalog columns are needed rather than one: `sys.columns.max_length`
/// is in **bytes**, so an `nvarchar(50)` reports 100, and only
/// `COLUMNPROPERTY(…, 'charmaxlen')` gives the character count — while
/// `max_length` is still the column that carries -1 as the `(max)` marker.
fn render_type_name(
    name: &str,
    max_length: i16,
    char_max_length: Option<i32>,
    precision: u8,
    scale: u8,
) -> String {
    match name {
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!("{name}({max_length})")
            }
        }
        "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!(
                    "{name}({})",
                    char_max_length.unwrap_or(max_length as i32 / 2)
                )
            }
        }
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        "float" => format!("float({precision})"),
        _ => name.to_string(),
    }
}

pub(crate) async fn databases(client: &mut Tds) -> Result<Vec<DatabaseInfo>, MsSqlError> {
    let rows = rows(
        client,
        "SELECT db.name, db.state_desc, db.collation_name \
         FROM sys.databases db \
         ORDER BY db.name",
        &[],
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| DatabaseInfo {
            name: text(r, 0),
            state: text(r, 1),
            collation: r.get::<&str, _>(2).map(str::to_string),
        })
        .collect())
}

pub(crate) async fn schemas(client: &mut Tds) -> Result<Vec<SchemaInfo>, MsSqlError> {
    // `sys.schemas` is not filtered by permission, so a login sees the names of
    // schemas it cannot read into. That matches what every other SQL Server
    // client shows, and the alternative — hiding a schema until the user is
    // granted something in it — makes a permission problem look like a missing
    // object.
    let sql = format!(
        "SELECT s.name FROM sys.schemas s WHERE s.name NOT IN ({HIDDEN_SCHEMAS}) ORDER BY s.name"
    );
    let rows = rows(client, &sql, &[]).await?;
    Ok(rows
        .iter()
        .map(|r| SchemaInfo { name: text(r, 0) })
        .collect())
}

pub(crate) async fn relations(
    client: &mut Tds,
    schema: &str,
) -> Result<Vec<RelationInfo>, MsSqlError> {
    // `sys.partitions` rather than `sys.dm_db_partition_stats`, which would give
    // the same number in one fewer join: the dynamic management view needs
    // VIEW DATABASE STATE, which an ordinary application login will not have,
    // and a navigator call that fails outright for want of a permission is worse
    // than one that answers without an estimate. `sys.partitions` is a catalog
    // view and simply shows fewer rows to a login that can see less.
    //
    // `index_id IN (0, 1)` is the heap or the clustered index — the one place a
    // table's rows are actually counted, rather than once per nonclustered index
    // as well.
    let rows = rows(
        client,
        "SELECT o.name, \
                o.type, \
                CAST(COALESCE(t.is_external, 0) AS bit) AS is_external, \
                ps.rows AS estimated_rows \
         FROM sys.objects o \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.tables t ON t.object_id = o.object_id \
         LEFT JOIN ( \
               SELECT p.object_id, SUM(p.rows) AS rows \
               FROM sys.partitions p \
               WHERE p.index_id IN (0, 1) \
               GROUP BY p.object_id \
         ) ps ON ps.object_id = o.object_id \
         WHERE s.name = @P1 AND o.type IN ('U', 'V') \
         ORDER BY o.name",
        &[&schema],
    )
    .await?;

    Ok(rows
        .iter()
        .map(|r| RelationInfo {
            schema: schema.to_string(),
            name: text(r, 0),
            kind: relation_kind(&text(r, 1), r.get(2).unwrap_or(false)),
            // Absent rather than zero for a view, or for a table whose
            // partitions this login cannot see. Microsoft documents this number
            // as "the approximate number of rows" with no condition under which
            // it is exact, so it belongs in `estimated_rows` and nowhere else —
            // and declining to answer is not the same as answering zero.
            estimated_rows: r.get(3),
        })
        .collect())
}

pub(crate) async fn columns(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Vec<ColumnInfo>, MsSqlError> {
    let rows = rows(
        client,
        "SELECT c.name, \
                ty.name AS type_name, \
                c.max_length, c.precision, c.scale, \
                COLUMNPROPERTY(c.object_id, c.name, 'charmaxlen') AS char_max_length, \
                c.is_nullable, \
                c.column_id, \
                dc.definition AS default_definition, \
                cc.definition AS computed_definition, \
                CAST(CASE WHEN pkc.column_id IS NULL THEN 0 ELSE 1 END AS bit) AS is_primary_key \
         FROM sys.columns c \
         JOIN sys.objects o ON o.object_id = c.object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
         LEFT JOIN sys.default_constraints dc \
                ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
         LEFT JOIN sys.computed_columns cc \
                ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN sys.indexes pk \
                ON pk.object_id = c.object_id AND pk.is_primary_key = 1 \
         LEFT JOIN sys.index_columns pkc \
                ON pkc.object_id = pk.object_id AND pkc.index_id = pk.index_id \
               AND pkc.column_id = c.column_id AND pkc.is_included_column = 0 \
         WHERE s.name = @P1 AND o.name = @P2 \
         ORDER BY c.column_id",
        &[&schema, &relation],
    )
    .await?;

    Ok(rows
        .iter()
        .enumerate()
        .map(|(offset, r)| ColumnInfo {
            name: text(r, 0),
            data_type: render_type_name(
                &text(r, 1),
                r.get(2).unwrap_or(0),
                r.get(5),
                r.get(3).unwrap_or(0),
                r.get(4).unwrap_or(0),
            ),
            nullable: r.get(6).unwrap_or(true),
            // `column_id` is 1-based already and would do, except that dropping
            // a column leaves a gap in it. The shared shape promises a position
            // that ascends by one, so it is counted here from the order the
            // catalog returned.
            position: offset as i32 + 1,
            is_primary_key: r.get(10).unwrap_or(false),
            // A computed column has no default and a default is not a
            // computation, so only one of the two is ever set. The expression
            // goes in this field because the shared shape has nowhere else for
            // it, and a structure pane showing nothing for a computed column
            // would be hiding the only interesting thing about it.
            default_value: r
                .get::<&str, _>(8)
                .or_else(|| r.get::<&str, _>(9))
                .map(str::to_string),
        })
        .collect())
}

/// The statement a view was created from, as the user typed it.
///
/// This is SQLite's behaviour rather than PostgreSQL's: `sys.sql_modules` keeps
/// the original source, including the `CREATE VIEW` header, the comments and the
/// whitespace, where `pg_get_viewdef` re-renders the query from its parse tree.
///
/// `None` covers two different things, and both are right. A table has no module
/// row at all, which is the distinction the structure pane hangs a section on;
/// and a view created `WITH ENCRYPTION` has a row whose definition is NULL,
/// which must not surface as an empty string — an empty box says "this view has
/// no body", and the truth is "the server will not show it to you".
pub(crate) async fn definition(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Option<String>, MsSqlError> {
    let rows = rows(
        client,
        "SELECT m.definition \
         FROM sys.sql_modules m \
         JOIN sys.objects o ON o.object_id = m.object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE s.name = @P1 AND o.name = @P2 AND o.type = 'V'",
        &[&schema, &relation],
    )
    .await?;
    Ok(rows
        .first()
        .and_then(|r| r.get::<&str, _>(0))
        .map(str::to_string))
}

/// UNIQUE keys that name columns, primary key excluded.
///
/// `sys.indexes` and not `sys.key_constraints`, because SQL Server enforces a
/// `UNIQUE` constraint with a unique index and `CREATE UNIQUE INDEX` produces
/// one that no constraint view lists. Both name a row equally well.
///
/// Two filters do the work `UniqueKeyInfo` describes. `filter_definition IS
/// NULL` drops the filtered index, which is unique over the rows it covers and
/// promises nothing about the ones it does not. `is_included_column = 0` drops
/// the `INCLUDE` payload, which is stored in the index without being part of
/// what the server keeps unique.
///
/// The sort direction is left off the column here, unlike in `indexes`: a
/// descending key is still an equality on that column, and `id DESC` in a
/// `WHERE` clause is a syntax error rather than a key.
pub(crate) async fn unique_keys(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Vec<UniqueKeyInfo>, MsSqlError> {
    let rows = rows(
        client,
        "SELECT i.name, c.name AS column_name \
         FROM sys.indexes i \
         JOIN sys.objects o ON o.object_id = i.object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.index_columns ic \
                ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns c \
                ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE s.name = @P1 AND o.name = @P2 \
           AND i.is_unique = 1 AND i.is_primary_key = 0 AND i.name IS NOT NULL \
           AND i.filter_definition IS NULL AND ic.is_included_column = 0 \
         ORDER BY i.name, ic.key_ordinal",
        &[&schema, &relation],
    )
    .await?;

    let mut keys: Vec<UniqueKeyInfo> = Vec::new();
    for r in &rows {
        let name = text(r, 0);
        // One row per key column, already grouped by the ORDER BY, so the last
        // key built is the one this row belongs to.
        match keys.last_mut() {
            Some(last) if last.name == name => last.columns.push(text(r, 1)),
            _ => keys.push(UniqueKeyInfo {
                name,
                columns: vec![text(r, 1)],
            }),
        }
    }
    Ok(keys)
}

pub(crate) async fn indexes(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Vec<IndexInfo>, MsSqlError> {
    // One row per index column, gathered below. `i.type <> 0` drops the heap,
    // which is a row in `sys.indexes` but is not an index; `i.name IS NOT NULL`
    // drops the same thing under its other guise.
    //
    // No `pg_get_indexdef`-per-position machinery, because SQL Server cannot
    // index an expression: it indexes a computed column, so `sys.columns.name`
    // always names something. The expression, when somebody wants it, is the
    // computed column's definition and arrives through `columns`.
    let rows = rows(
        client,
        "SELECT i.name, \
                i.is_unique, i.is_primary_key, i.type_desc, i.filter_definition, \
                ic.key_ordinal, ic.is_descending_key, ic.is_included_column, \
                c.name AS column_name \
         FROM sys.indexes i \
         JOIN sys.objects o ON o.object_id = i.object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.index_columns ic \
                ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         LEFT JOIN sys.columns c \
                ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE s.name = @P1 AND o.name = @P2 \
           AND i.type <> 0 AND i.name IS NOT NULL \
         ORDER BY i.is_primary_key DESC, i.name, ic.is_included_column, ic.key_ordinal",
        &[&schema, &relation],
    )
    .await?;

    let mut out: Vec<IndexInfo> = Vec::new();
    for r in &rows {
        let name = text(r, 0);
        if out.last().map(|i| i.name.as_str()) != Some(name.as_str()) {
            out.push(IndexInfo {
                name,
                columns: Vec::new(),
                is_unique: r.get(1).unwrap_or(false),
                is_primary: r.get(2).unwrap_or(false),
                // The server's own word — CLUSTERED, NONCLUSTERED COLUMNSTORE,
                // SPATIAL, XML — rather than a table of our own that would have
                // to be kept in step with a new index kind.
                method: text(r, 3),
                predicate: r.get::<&str, _>(4).map(str::to_string),
            });
        }
        let index = out.last_mut().expect("just pushed");
        // Included columns are deliberately dropped rather than listed here.
        // They have `key_ordinal = 0` and cannot be seeked on, and the shared
        // shape documents this field as the keys the planner can use — putting a
        // payload column in it would misstate what the index is good for. The
        // information has nowhere else to go, so for now it is lost.
        if !r.get(7).unwrap_or(false) {
            let column = text(r, 8);
            if r.get(6).unwrap_or(false) {
                index.columns.push(format!("{column} DESC"));
            } else {
                index.columns.push(column);
            }
        }
    }
    Ok(out)
}

/// Foreign keys this relation declares.
pub(crate) async fn foreign_keys(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, MsSqlError> {
    relationships(client, schema, relation, Direction::Outbound).await
}

/// Foreign keys other relations declare against this one.
pub(crate) async fn referenced_by(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Vec<RelationshipInfo>, MsSqlError> {
    relationships(client, schema, relation, Direction::Inbound).await
}

enum Direction {
    Outbound,
    Inbound,
}

async fn relationships(
    client: &mut Tds,
    schema: &str,
    relation: &str,
    direction: Direction,
) -> Result<Vec<RelationshipInfo>, MsSqlError> {
    // Two statements rather than one parameterised by direction: the sides swap
    // in four places, and a query that decides which side is "local" from a flag
    // is one edit away from reporting a key backwards.
    //
    // `constraint_column_id` order in both, because a composite key's columns
    // have to line up with the ones they reference and no other order does that.
    let sql = match direction {
        Direction::Outbound => {
            "SELECT fk.name, \
                    pc.name AS local_column, \
                    rs.name AS other_schema, \
                    ro.name AS other_table, \
                    rc.name AS other_column, \
                    fk.update_referential_action, \
                    fk.delete_referential_action \
             FROM sys.foreign_keys fk \
             JOIN sys.objects o ON o.object_id = fk.parent_object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
                                AND pc.column_id = fkc.parent_column_id \
             JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
             JOIN sys.schemas rs ON rs.schema_id = ro.schema_id \
             JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
                                AND rc.column_id = fkc.referenced_column_id \
             WHERE s.name = @P1 AND o.name = @P2 \
             ORDER BY fk.name, fkc.constraint_column_id"
        }
        Direction::Inbound => {
            "SELECT fk.name, \
                    rc.name AS local_column, \
                    s.name AS other_schema, \
                    o.name AS other_table, \
                    pc.name AS other_column, \
                    fk.update_referential_action, \
                    fk.delete_referential_action \
             FROM sys.foreign_keys fk \
             JOIN sys.objects o ON o.object_id = fk.parent_object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
                                AND pc.column_id = fkc.parent_column_id \
             JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
             JOIN sys.schemas rs ON rs.schema_id = ro.schema_id \
             JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
                                AND rc.column_id = fkc.referenced_column_id \
             WHERE rs.name = @P1 AND ro.name = @P2 \
             ORDER BY s.name, o.name, fk.name, fkc.constraint_column_id"
        }
    };

    let rows = rows(client, sql, &[&schema, &relation]).await?;
    let mut out: Vec<RelationshipInfo> = Vec::new();
    for r in &rows {
        let name = text(r, 0);
        if out.last().map(|k| k.name.as_str()) != Some(name.as_str()) {
            out.push(RelationshipInfo {
                name,
                local_columns: Vec::new(),
                other_schema: text(r, 2),
                other_table: text(r, 3),
                other_columns: Vec::new(),
                on_update: referential_action(r.get(5).unwrap_or(0)),
                on_delete: referential_action(r.get(6).unwrap_or(0)),
            });
        }
        let key = out.last_mut().expect("just pushed");
        key.local_columns.push(text(r, 1));
        key.other_columns.push(text(r, 4));
    }
    Ok(out)
}

pub(crate) async fn constraints(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Vec<ConstraintInfo>, MsSqlError> {
    // Primary and foreign keys are left out for the reason the PostgreSQL driver
    // gives: both already have a section of their own, and listing a key twice
    // invites the reader to wonder whether they are two different things.
    //
    // `FOR XML PATH` rather than `STRING_AGG`, which is cleaner and needs
    // SQL Server 2017. Upstream picks between them on the server version; one
    // query that works from 2005 is worth more here than a version probe on
    // every call.
    //
    // No EXCLUDE: SQL Server has no exclusion constraints.
    let rows = rows(
        client,
        "SELECT cc.name, 'check' AS kind, cc.definition \
         FROM sys.check_constraints cc \
         JOIN sys.objects o ON o.object_id = cc.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE s.name = @P1 AND o.name = @P2 \
         UNION ALL \
         SELECT kc.name, 'unique', \
                'UNIQUE (' + STUFF(( \
                    SELECT ', ' + c.name \
                    FROM sys.index_columns ic \
                    JOIN sys.columns c ON c.object_id = ic.object_id \
                                      AND c.column_id = ic.column_id \
                    WHERE ic.object_id = kc.parent_object_id \
                      AND ic.index_id = kc.unique_index_id \
                      AND ic.is_included_column = 0 \
                    ORDER BY ic.key_ordinal \
                    FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, '') + ')' \
         FROM sys.key_constraints kc \
         JOIN sys.objects o ON o.object_id = kc.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE s.name = @P1 AND o.name = @P2 AND kc.type = 'UQ' \
         ORDER BY 2, 1",
        &[&schema, &relation],
    )
    .await?;

    Ok(rows
        .iter()
        .map(|r| ConstraintInfo {
            name: text(r, 0),
            kind: match text(r, 1).as_str() {
                "check" => ConstraintKind::Check,
                "unique" => ConstraintKind::Unique,
                _ => ConstraintKind::Other,
            },
            // The server's own rendering. Rebuilding a CHECK expression from
            // catalog columns would mean reimplementing expression formatting,
            // and getting it subtly wrong on the cases that matter.
            definition: text(r, 2),
        })
        .collect())
}

pub(crate) async fn triggers(
    client: &mut Tds,
    schema: &str,
    relation: &str,
) -> Result<Vec<TriggerInfo>, MsSqlError> {
    // `tr.is_ms_shipped = 0` is this driver's `NOT tgisinternal`: it keeps the
    // list to triggers somebody wrote.
    let rows = rows(
        client,
        "SELECT tr.name, \
                tr.is_instead_of_trigger, \
                tr.is_disabled, \
                STUFF((SELECT ', ' + te.type_desc \
                       FROM sys.trigger_events te \
                       WHERE te.object_id = tr.object_id \
                       FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, '') AS events, \
                m.definition \
         FROM sys.triggers tr \
         JOIN sys.objects o ON o.object_id = tr.parent_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.sql_modules m ON m.object_id = tr.object_id \
         WHERE s.name = @P1 AND o.name = @P2 AND tr.is_ms_shipped = 0 \
         ORDER BY tr.name",
        &[&schema, &relation],
    )
    .await?;

    Ok(rows
        .iter()
        .map(|r| TriggerInfo {
            name: text(r, 0),
            // SQL Server has no BEFORE trigger, so these are the only two
            // answers there are.
            timing: Some(if r.get(1).unwrap_or(false) {
                "INSTEAD OF".to_string()
            } else {
                "AFTER".to_string()
            }),
            events: text(r, 3)
                .split(", ")
                .filter(|e| !e.is_empty())
                .map(str::to_string)
                .collect(),
            // Always STATEMENT: SQL Server has no row-level DML trigger. The
            // `inserted` and `deleted` pseudo-tables are set-valued and a trigger
            // fires once per statement however many rows it touched, which is the
            // single most common thing somebody coming from PostgreSQL gets
            // wrong.
            level: Some("STATEMENT".to_string()),
            // There is none to name. A SQL Server trigger's body is inline, so
            // the shared shape's `function` stays empty and `definition` carries
            // the whole thing.
            function: None,
            enabled: !r.get(2).unwrap_or(false),
            definition: r.get::<&str, _>(4).map(str::to_string),
        })
        .collect())
}

/// A column that the catalog declares NOT NULL, read without an `unwrap`.
///
/// Every one of these is `sysname`, which is `nvarchar(128) NOT NULL`, so the
/// `None` arm never happens — but a metadata call that panicked on an unexpected
/// NULL would take down a navigator refresh, and an empty string in a tree is a
/// far smaller problem than that.
fn text(row: &Row, idx: usize) -> String {
    row.get::<&str, _>(idx).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_n_prefixed_type_is_measured_in_characters() {
        // `sys.columns.max_length` is in bytes, so an nvarchar(50) reports 100.
        // Rendering that number would tell the user their column is twice the
        // size they declared.
        assert_eq!(
            render_type_name("nvarchar", 100, Some(50), 0, 0),
            "nvarchar(50)"
        );
        assert_eq!(render_type_name("nchar", 20, Some(10), 0, 0), "nchar(10)");
        // A single-byte type measures the same either way.
        assert_eq!(
            render_type_name("varchar", 16, Some(16), 0, 0),
            "varchar(16)"
        );
    }

    #[test]
    fn a_max_length_of_minus_one_is_the_max_marker() {
        assert_eq!(
            render_type_name("nvarchar", -1, None, 0, 0),
            "nvarchar(max)"
        );
        assert_eq!(
            render_type_name("varbinary", -1, None, 0, 0),
            "varbinary(max)"
        );
        assert_eq!(render_type_name("varchar", -1, None, 0, 0), "varchar(max)");
    }

    #[test]
    fn a_decimal_states_both_of_its_numbers() {
        assert_eq!(render_type_name("decimal", 9, None, 18, 4), "decimal(18,4)");
        assert_eq!(render_type_name("numeric", 5, None, 5, 0), "numeric(5,0)");
    }

    #[test]
    fn a_fractional_time_states_its_scale() {
        assert_eq!(
            render_type_name("datetime2", 8, None, 27, 7),
            "datetime2(7)"
        );
        assert_eq!(render_type_name("time", 5, None, 16, 3), "time(3)");
        assert_eq!(
            render_type_name("datetimeoffset", 10, None, 34, 7),
            "datetimeoffset(7)"
        );
    }

    #[test]
    fn a_type_with_nothing_to_add_is_left_as_it_is() {
        for name in [
            "int",
            "bit",
            "money",
            "uniqueidentifier",
            "xml",
            "timestamp",
        ] {
            assert_eq!(render_type_name(name, 8, None, 0, 0), name);
        }
    }

    #[test]
    fn referential_actions_are_spelled_the_way_the_ddl_spells_them() {
        assert_eq!(referential_action(0), "NO ACTION");
        assert_eq!(referential_action(1), "CASCADE");
        assert_eq!(referential_action(2), "SET NULL");
        assert_eq!(referential_action(3), "SET DEFAULT");
        // A code nobody has heard of is read as the action that does nothing,
        // which is the one that cannot surprise somebody reading the tree.
        assert_eq!(referential_action(9), "NO ACTION");
    }

    #[test]
    fn an_external_table_is_a_foreign_table_whatever_its_object_type_says() {
        assert_eq!(relation_kind("U", false), RelationKind::Table);
        assert_eq!(relation_kind("V", false), RelationKind::View);
        assert_eq!(relation_kind("U", true), RelationKind::ForeignTable);
        // `sys.objects.type` is char(2) and arrives padded.
        assert_eq!(relation_kind("U ", false), RelationKind::Table);
        assert_eq!(relation_kind("P", false), RelationKind::Unknown);
    }
}
