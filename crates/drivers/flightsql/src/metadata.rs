//! What a Flight SQL server can say about itself, asked in the protocol's own
//! words.
//!
//! This is the choice the driver was written to make. Every server behind this
//! protocol has an engine — DuckDB here, but it could be Spark, Dremio, Postgres
//! or something nobody has written yet — and `information_schema` is that
//! engine's answer rather than the protocol's. So nothing here sends SQL. Five
//! of the nine calls are `CommandGet…` messages, and the other four answer with
//! nothing because the protocol has no command to ask with, and reaching around
//! it would make this driver work against exactly one server.
//!
//! What the protocol has, and what this server does with it:
//!
//! - `CommandGetDbSchemas` — works, with one wrinkle: its `catalog` filter is
//!   ignored, so the same three schemas come back whichever catalog is named.
//!   The rows carry `catalog_name` themselves, so the filtering is redone here
//!   on what came back.
//! - `CommandGetTables` — works, filters honoured, and `include_schema` returns
//!   the table's Arrow schema as an IPC blob. That last is what makes `columns`
//!   possible without SQL.
//! - `CommandGetPrimaryKeys`, `CommandGetImportedKeys`, `CommandGetExportedKeys`
//!   — all three work, and all three answer with no rows for a table that is not
//!   there, which is what the navigator needs.
//! - `CommandGetXdbcTypeInfo` — **not implemented**: this server answers
//!   `Unimplemented` to it. Nothing here calls it, so nothing is lost; it is
//!   recorded because a driver that used it to name column types would fail
//!   against this server and nowhere else.
//!
//! **Four calls answer with nothing and issue no request to find out.** Each is a
//! statement about the protocol rather than a gap here. Flight SQL has no command
//! for a view's text, for an index, for a constraint or for a trigger — the
//! `CommandGet…` set is catalogs, schemas, tables, table types, primary keys,
//! imported keys, exported keys, cross references, SQL info and XDBC type info,
//! and that is the whole of it. A driver could get all four out of
//! `information_schema` against this server, and would then be a DuckDB driver
//! that happens to speak Flight SQL.
//!
//! The filters are `LIKE` patterns and the protocol defines no escape for them,
//! so a schema called `a_b` cannot be asked for exactly — `_` matches any
//! character. Every listing here therefore checks the names that came back rather
//! than trusting the pattern, which costs nothing and is the only way to be sure
//! the answer is about the relation that was asked for.

use arrow::array::{Array, BinaryArray, Int32Array, RecordBatch, StringArray, UInt8Array};
use arrow_flight::IpcMessage;
use arrow_flight::sql::{
    CommandGetDbSchemas, CommandGetExportedKeys, CommandGetImportedKeys, CommandGetPrimaryKeys,
    CommandGetTables,
};
use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    SchemaInfo, TriggerInfo, UniqueKeyInfo,
};
use std::sync::Arc;

use crate::{FlightSqlError, FlightSqlSource, Rows, Stop};

/// What the Flight SQL spec calls the engine's own name for a column's type,
/// where the server bothers to say.
///
/// Optional in the protocol and absent on this server, which is why `columns`
/// reports the Arrow type instead. Read anyway, because it is the protocol's
/// answer to a question the Arrow type only approximates — a `Utf8` that is a
/// `VARCHAR(64)` is a fact the structure pane wants and the Arrow schema has
/// thrown away.
const TYPE_NAME: &str = "ARROW:FLIGHT:SQL:TYPE_NAME";

/// How the protocol numbers a referential action, from `FlightSql.proto`.
///
/// The numbers are the protocol's and not JDBC's, though they were taken from
/// it; they are written out here rather than left as integers because
/// `RelationshipInfo` carries the words a structure pane shows.
fn rule(code: u8) -> String {
    match code {
        0 => "CASCADE",
        1 => "RESTRICT",
        2 => "SET NULL",
        3 => "NO ACTION",
        4 => "SET DEFAULT",
        _ => "",
    }
    .to_string()
}

impl FlightSqlSource {
    /// Runs one metadata command and collects every row it produced.
    ///
    /// Through `Rows`, which is the one decode path this driver has: a metadata
    /// result is a Flight stream like any other, and a second reader for it would
    /// be a second thing to keep right.
    async fn ask(
        &self,
        info: arrow_flight::FlightInfo,
    ) -> Result<Vec<RecordBatch>, FlightSqlError> {
        let mut rows = Rows::from_info(
            self.client(),
            info,
            1024,
            Arc::clone(&self.stop_for_metadata()),
        )
        .await?;
        let mut out = Vec::new();
        while let Some(batch) = rows.next_page().await? {
            out.push(batch);
        }
        Ok(out)
    }

    /// The schemas this connection can see, as `catalog.schema`.
    ///
    /// Two levels flattened into the trait's one, the way the DuckDB driver
    /// flattens its own catalog level — and for the same reason: Flight SQL has a
    /// catalog above the schema, and two schemas called `main` in different
    /// catalogs are different schemas. A server that reports no catalog gets the
    /// bare schema name, which is what most of them are.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, FlightSqlError> {
        let info = self
            .client()
            .get_db_schemas(CommandGetDbSchemas {
                // Sent even though this server ignores it: a filter the server
                // honours is a filter it does not have to send rows for.
                catalog: (!self.catalog().is_empty()).then(|| self.catalog().to_string()),
                db_schema_filter_pattern: None,
            })
            .await
            .map_err(crate::server_said)?;

        let mut out = Vec::new();
        for batch in self.ask(info).await? {
            let catalogs = text(&batch, "catalog_name")?;
            let schemas = text(&batch, "db_schema_name")?;
            for row in 0..batch.num_rows() {
                let catalog = value(catalogs, row);
                if !self.catalog().is_empty() && catalog != self.catalog() {
                    continue;
                }
                // False for everything, and not because none of them is: the
                // protocol has no field saying which, and behind it may be any
                // engine at all. Guessing at names would be this driver
                // pretending to know which product answered — the same reason
                // `reports_routines` is false here.
                out.push(SchemaInfo {
                    name: qualified(catalog, value(schemas, row)),
                    is_system: false,
                });
            }
        }
        Ok(out)
    }

    /// The tables and views in one schema.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, FlightSqlError> {
        let mut out = Vec::new();
        for batch in self.tables(schema, None, false).await? {
            let catalogs = text(&batch, "catalog_name")?;
            let schemas = text(&batch, "db_schema_name")?;
            let names = text(&batch, "table_name")?;
            let kinds = text(&batch, "table_type")?;
            for row in 0..batch.num_rows() {
                if qualified(value(catalogs, row), value(schemas, row)) != schema {
                    continue;
                }
                out.push(RelationInfo {
                    schema: schema.to_string(),
                    name: value(names, row).to_string(),
                    kind: kind(value(kinds, row)),
                    // Deliberately not filled. The protocol has no row count for
                    // a table — `FlightInfo::total_records` is about one result
                    // and this server sends -1 for that too — so `None` means
                    // "nothing has measured this", which is exactly the case.
                    estimated_rows: None,
                });
            }
        }
        Ok(out)
    }

    /// The columns of one relation, out of the Arrow schema the protocol carries.
    ///
    /// `CommandGetTables` with `include_schema` answers with the table's schema
    /// as an IPC blob, which is the protocol's own description of a relation's
    /// columns. `data_type` is therefore the Arrow type unless the server
    /// attached its engine's own name — `ColumnInfo::data_type` asks for "the
    /// type as the database states it", and against a Flight SQL server the Arrow
    /// type *is* what the database states, since it is what the values will
    /// arrive as. This server sets no `ARROW:FLIGHT:SQL:TYPE_NAME`, so `Utf8` is
    /// what a structure pane shows here.
    ///
    /// Two round trips, because the primary key is a separate command. Worth it:
    /// which columns are the key decides whether a grid can write a row back.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, FlightSqlError> {
        let batches = self.tables(schema, Some(relation), true).await?;
        let mut out = Vec::new();
        for batch in &batches {
            let catalogs = text(batch, "catalog_name")?;
            let schemas = text(batch, "db_schema_name")?;
            let names = text(batch, "table_name")?;
            let blobs = binary(batch, "table_schema")?;
            for row in 0..batch.num_rows() {
                if qualified(value(catalogs, row), value(schemas, row)) != schema
                    || value(names, row) != relation
                {
                    continue;
                }
                let ipc = IpcMessage(blobs.value(row).to_vec().into());
                let described = arrow::datatypes::Schema::try_from(ipc)?;
                for (at, field) in described.fields().iter().enumerate() {
                    out.push(ColumnInfo {
                        name: field.name().clone(),
                        data_type: field
                            .metadata()
                            .get(TYPE_NAME)
                            .cloned()
                            .unwrap_or_else(|| field.data_type().to_string()),
                        nullable: field.is_nullable(),
                        position: at as i32 + 1,
                        is_primary_key: false,
                        // The protocol carries no column default. Reporting one
                        // would mean asking the engine, which is the thing this
                        // driver does not do.
                        default_value: None,
                        computed: None,
                    });
                }
            }
        }
        if out.is_empty() {
            return Ok(out);
        }

        let keys = self.primary_key(schema, relation).await?;
        for column in &mut out {
            column.is_primary_key = keys.iter().any(|key| key == &column.name);
        }
        Ok(out)
    }

    /// Always `None`, and without asking.
    ///
    /// Flight SQL has no command for the statement a view is defined by.
    /// `CommandGetTables` says a relation is a `VIEW` and stops there. The text
    /// is in the engine's catalog and reading it would mean sending
    /// engine-specific SQL, which is the one thing this driver declines to do.
    pub async fn definition(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Option<String>, FlightSqlError> {
        Ok(None)
    }

    /// Empty, always, and without asking. The `CommandGet…` set has a call for
    /// the primary key and for both directions of a foreign key, and none for a
    /// unique constraint — so a server may well enforce one and the protocol has
    /// no way to say so. Answering from the primary key alone is what the
    /// protocol supports; inventing the rest is what it does not.
    pub async fn unique_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, FlightSqlError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Flight SQL has no command for an index:
    /// the `CommandGet…` set covers catalogs, schemas, tables, table types and
    /// keys, and nothing else about how a relation is stored.
    pub async fn indexes(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<IndexInfo>, FlightSqlError> {
        Ok(Vec::new())
    }

    /// The foreign keys this relation declares.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, FlightSqlError> {
        let (catalog, db_schema) = split(schema);
        let info = self
            .client()
            .get_imported_keys(CommandGetImportedKeys {
                catalog: catalog.map(str::to_string),
                db_schema: Some(db_schema.to_string()),
                table: relation.to_string(),
            })
            .await
            .map_err(crate::server_said)?;
        self.relationships(info, "pk").await
    }

    /// The foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, FlightSqlError> {
        let (catalog, db_schema) = split(schema);
        let info = self
            .client()
            .get_exported_keys(CommandGetExportedKeys {
                catalog: catalog.map(str::to_string),
                db_schema: Some(db_schema.to_string()),
                table: relation.to_string(),
            })
            .await
            .map_err(crate::server_said)?;
        self.relationships(info, "fk").await
    }

    /// Empty, always, and without asking. The protocol reports primary keys and
    /// foreign keys and has no command for a unique, check or exclusion
    /// constraint — so there is nothing here that is not already on the columns.
    pub async fn constraints(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<ConstraintInfo>, FlightSqlError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Flight SQL has no notion of a trigger.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, FlightSqlError> {
        Ok(Vec::new())
    }

    /// One `CommandGetTables`, filtered to a schema and optionally to a relation.
    async fn tables(
        &self,
        schema: &str,
        relation: Option<&str>,
        include_schema: bool,
    ) -> Result<Vec<RecordBatch>, FlightSqlError> {
        let (catalog, db_schema) = split(schema);
        let info = self
            .client()
            .get_tables(CommandGetTables {
                catalog: catalog.map(str::to_string),
                db_schema_filter_pattern: Some(db_schema.to_string()),
                table_name_filter_pattern: relation.map(str::to_string),
                // Every kind. A navigator shows views beside tables, and
                // `CommandGetTableTypes` is what would name them — this driver
                // does not need to, because `table_type` comes back on every row.
                table_types: Vec::new(),
                include_schema,
            })
            .await
            .map_err(crate::server_said)?;
        self.ask(info).await
    }

    /// The columns of a relation's primary key, in key order.
    async fn primary_key(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<String>, FlightSqlError> {
        let (catalog, db_schema) = split(schema);
        let info = self
            .client()
            .get_primary_keys(CommandGetPrimaryKeys {
                catalog: catalog.map(str::to_string),
                db_schema: Some(db_schema.to_string()),
                table: relation.to_string(),
            })
            .await
            .map_err(crate::server_said)?;

        let mut found: Vec<(i32, String)> = Vec::new();
        for batch in self.ask(info).await? {
            let columns = text(&batch, "column_name")?;
            let order = batch
                .column_by_name("key_sequence")
                .and_then(|c| c.as_any().downcast_ref::<Int32Array>());
            for row in 0..batch.num_rows() {
                found.push((
                    order.map_or(row as i32, |o| o.value(row)),
                    value(columns, row).to_string(),
                ));
            }
        }
        found.sort_by_key(|(at, _)| *at);
        Ok(found.into_iter().map(|(_, name)| name).collect())
    }

    /// The imported- or exported-keys result, read into `RelationshipInfo`.
    ///
    /// One reader for both because the protocol gives them the same thirteen
    /// columns; what differs is which end is "the other one", which is what
    /// `other` names. A key spanning several columns arrives as one row per
    /// column with a `key_sequence`, so the rows are gathered by key name.
    async fn relationships(
        &self,
        info: arrow_flight::FlightInfo,
        other: &str,
    ) -> Result<Vec<RelationshipInfo>, FlightSqlError> {
        let mut out: Vec<RelationshipInfo> = Vec::new();
        for batch in self.ask(info).await? {
            let name = text(&batch, "fk_key_name")?;
            let local = text(
                &batch,
                if other == "pk" {
                    "fk_column_name"
                } else {
                    "pk_column_name"
                },
            )?;
            let other_catalog = text(&batch, &format!("{other}_catalog_name"))?;
            let other_schema = text(&batch, &format!("{other}_db_schema_name"))?;
            let other_table = text(&batch, &format!("{other}_table_name"))?;
            let other_column = text(&batch, &format!("{other}_column_name"))?;
            let update = rules(&batch, "update_rule")?;
            let delete = rules(&batch, "delete_rule")?;

            for row in 0..batch.num_rows() {
                let key = value(name, row).to_string();
                let table = value(other_table, row).to_string();
                let existing = out
                    .iter_mut()
                    .find(|r| r.name == key && r.other_table == table);
                match existing {
                    Some(relationship) => {
                        relationship
                            .local_columns
                            .push(value(local, row).to_string());
                        relationship
                            .other_columns
                            .push(value(other_column, row).to_string());
                    }
                    None => out.push(RelationshipInfo {
                        name: key,
                        local_columns: vec![value(local, row).to_string()],
                        other_schema: qualified(
                            value(other_catalog, row),
                            value(other_schema, row),
                        ),
                        other_table: table,
                        other_columns: vec![value(other_column, row).to_string()],
                        on_update: rule(update.value(row)),
                        on_delete: rule(delete.value(row)),
                    }),
                }
            }
        }
        Ok(out)
    }

    /// The stop a metadata call reads under.
    ///
    /// The session's own, so that Cancel reaches a navigator refresh as well as a
    /// statement — which is what "abandon whatever this session is running"
    /// means, and what the ClickHouse driver's `KILL QUERY` covers too.
    fn stop_for_metadata(&self) -> Arc<Stop> {
        Arc::clone(&self.stop)
    }
}

/// `catalog.schema`, or the schema alone where the server reports no catalog.
fn qualified(catalog: &str, schema: &str) -> String {
    if catalog.is_empty() {
        schema.to_string()
    } else {
        format!("{catalog}.{schema}")
    }
}

/// The two halves of a name `qualified` made, at the first dot.
///
/// The one place this driver loses information, and the price of the trait having
/// one namespace level where Flight SQL has two: a catalog whose name contains a
/// dot is split in the wrong place. The DuckDB driver had the same problem and no
/// longer has it — its catalog level moved to `Driver::databases`, and a session
/// is moved onto one with `USE`. Flight SQL has no such command: a catalog is a
/// field of every metadata call rather than somewhere a connection can be.
fn split(schema: &str) -> (Option<&str>, &str) {
    match schema.split_once('.') {
        Some((catalog, rest)) => (Some(catalog), rest),
        None => (None, schema),
    }
}

/// What the protocol's `table_type` means to a navigator.
///
/// The strings are the server's and the protocol does not fix them —
/// `CommandGetTableTypes` exists precisely because each server has its own list.
/// So this reads the shapes that recur rather than an exhaustive table, and
/// anything else is `Unknown` rather than guessed at.
fn kind(table_type: &str) -> RelationKind {
    match table_type.to_ascii_uppercase().as_str() {
        "TABLE" | "BASE TABLE" | "LOCAL TEMPORARY" | "SYSTEM TABLE" => RelationKind::Table,
        "VIEW" | "SYSTEM VIEW" => RelationKind::View,
        "MATERIALIZED VIEW" => RelationKind::MaterializedView,
        "FOREIGN TABLE" => RelationKind::ForeignTable,
        _ => RelationKind::Unknown,
    }
}

/// One text column of a metadata result.
///
/// By name rather than by position: the protocol fixes the names and lists the
/// columns in an order a server is not obliged to keep, and a driver reading
/// column 3 would quietly read the wrong one against a server that ordered them
/// differently.
fn text<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray, FlightSqlError> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| {
            FlightSqlError::Server(format!(
                "this server's metadata result has no text column called {name}"
            ))
        })
}

fn binary<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a BinaryArray, FlightSqlError> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<BinaryArray>())
        .ok_or_else(|| {
            FlightSqlError::Server(format!(
                "this server's metadata result has no binary column called {name}"
            ))
        })
}

fn rules<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt8Array, FlightSqlError> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<UInt8Array>())
        .ok_or_else(|| {
            FlightSqlError::Server(format!(
                "this server's key result has no {name} column of bytes"
            ))
        })
}

/// One cell, with a null read as the empty string.
///
/// `catalog_name` and `db_schema_name` are nullable in every one of these
/// results, because a server with no catalogs has nothing to put there. Empty is
/// what `qualified` then leaves out of the name, so the two agree.
fn value(column: &StringArray, row: usize) -> &str {
    if column.is_null(row) {
        ""
    } else {
        column.value(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves of the trait's one schema string, and the case that loses.
    #[test]
    fn a_qualified_name_splits_back_into_the_two_levels_it_was_made_from() {
        assert_eq!(split("TPC-H-small.main"), (Some("TPC-H-small"), "main"));
        assert_eq!(split("main"), (None, "main"));
        assert_eq!(qualified("TPC-H-small", "main"), "TPC-H-small.main");
        // A server with no catalog level gets the bare schema, both ways.
        assert_eq!(qualified("", "main"), "main");
        // And the documented loss: the dot in the catalog wins.
        assert_eq!(split("sales.2024.main"), (Some("sales"), "2024.main"));
    }

    /// The protocol's referential actions, which are numbers on the wire and
    /// words in a structure pane.
    #[test]
    fn a_referential_action_is_reported_by_the_word_the_protocol_numbers() {
        assert_eq!(rule(0), "CASCADE");
        assert_eq!(rule(3), "NO ACTION");
        // Not a rule this protocol defines: reported as nothing rather than as
        // whichever word happened to be nearest.
        assert_eq!(rule(9), "");
    }

    /// A relation kind a navigator draws an icon for, from a string the protocol
    /// deliberately does not fix.
    #[test]
    fn a_table_type_this_protocol_does_not_fix_is_not_guessed_at() {
        assert_eq!(kind("BASE TABLE"), RelationKind::Table);
        assert_eq!(kind("view"), RelationKind::View);
        assert_eq!(kind("STREAMING TABLE"), RelationKind::Unknown);
    }
}
