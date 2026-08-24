//! What Unity Catalog can say about itself, in the shape the navigator expects.
//!
//! One kind of statement: Unity Catalog publishes a standard `information_schema`
//! in every catalog, so unlike Snowflake there is no second family of `SHOW`
//! commands to fall back on for keys. `TABLE_CONSTRAINTS`, `KEY_COLUMN_USAGE`,
//! `REFERENTIAL_CONSTRAINTS` and `CHECK_CONSTRAINTS` are all there, which makes
//! the key queries ordinary joins rather than several answers stitched together.
//!
//! **`system.information_schema` is the one place that answers for the whole
//! metastore**, and `schemas()` is built on it. Every other view here is
//! per-catalog — `main.information_schema.tables` describes `main` and nothing
//! else — so listing the schemas of every catalog any other way is one round trip
//! per catalog. That is the same trade the Trino driver makes when it reaches for
//! `system.jdbc.schemas` instead of one `information_schema` per catalog. It is
//! also the single most likely thing in this driver to fail outright: a metastore
//! whose system catalog has not been enabled answers with a permission error, and
//! the navigator's root is then empty with that message behind it.
//!
//! **Two calls answer with nothing and send no statement to find out.**
//!
//! - There are no indexes. A Delta table's read performance comes from file
//!   statistics, data skipping and clustering — `OPTIMIZE`, `ZORDER BY`, liquid
//!   clustering — none of which is an object beside the table, and all of which
//!   are properties of how the files were written. There is no `CREATE INDEX` and
//!   no catalog view for one.
//! - There are no triggers. Databricks' answer to the same need is a job, a
//!   Delta Live Tables pipeline or a file trigger, all of which are scheduled
//!   things outside the table, and putting them in a section labelled for
//!   triggers would name three unrelated features as one.
//!
//! **Column positions are numbered here rather than read.** Every other driver
//! takes `ordinal_position` from the catalog and converts if it counts from zero.
//! Which base Unity Catalog uses is something no server has told this driver, so
//! the statement orders by it and the position is the row's place in that order —
//! one-based by construction. It costs nothing and removes a question that would
//! otherwise be a silent off-by-one in every structure pane.
//!
//! Nothing in this file has been run.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo,
};

use crate::{DatabricksError, DatabricksSource, literal, parts, quote};

/// `information_schema.tables.table_type`, as one of the kinds the navigator
/// knows about.
///
/// A managed table and an external table are both tables — the difference is
/// whether Unity Catalog owns the storage, which is not something a structure
/// pane acts on. `FOREIGN` is the one that really is somewhere else: it is a
/// table in another system reached through Lakehouse Federation, which is what
/// `ForeignTable` means everywhere else in this trait.
///
/// A streaming table is deliberately `Unknown` rather than mapped to the nearest
/// thing. It is continuously refreshed by a pipeline that owns it, so offering a
/// table's actions for one would offer to edit rows that the next refresh
/// overwrites — and there is no kind in the trait that says what it is.
fn relation_kind(table_type: &str) -> RelationKind {
    match table_type {
        "MANAGED" | "EXTERNAL" => RelationKind::Table,
        "VIEW" => RelationKind::View,
        "MATERIALIZED_VIEW" => RelationKind::MaterializedView,
        "FOREIGN" => RelationKind::ForeignTable,
        _ => RelationKind::Unknown,
    }
}

impl DatabricksSource {
    /// Every schema in the metastore, as `catalog.schema`.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, DatabricksError> {
        let answer = self
            .ask(
                "SELECT catalog_name, schema_name FROM system.information_schema.schemata \
                 ORDER BY catalog_name, schema_name",
            )
            .await?;
        let catalog = answer.at("catalog_name");
        let schema = answer.at("schema_name");
        Ok(answer
            .rows()
            .iter()
            // `catalog.schema`, in halves. The `system` catalog is the one
            // this very query reads, and each catalog carries an
            // `information_schema` of its own.
            .map(|row| {
                let (catalog, schema) = (answer.text(row, catalog), answer.text(row, schema));
                SchemaInfo {
                    is_system: catalog == "system" || schema == "information_schema",
                    name: format!("{catalog}.{schema}"),
                }
            })
            .collect())
    }

    /// The tables and views in one `catalog.schema`.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, DatabricksError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let answer = self
            .ask(&format!(
                "SELECT table_name, table_type FROM {}.information_schema.tables \
                 WHERE table_schema = {} ORDER BY table_name",
                quote(catalog),
                literal(inner)
            ))
            .await?;
        let name = answer.at("table_name");
        let kind = answer.at("table_type");
        Ok(answer
            .rows()
            .iter()
            .map(|row| RelationInfo {
                schema: schema.to_string(),
                name: answer.text(row, name),
                kind: relation_kind(&answer.text(row, kind)),
                // Deliberately not filled. Unity Catalog's `information_schema`
                // has no row count, and the numbers that exist — `DESCRIBE
                // DETAIL`, `ANALYZE TABLE` statistics — are one statement per
                // table against a warehouse that charges for the time. A
                // navigator expanding a schema of two hundred tables would issue
                // two hundred of them.
                estimated_rows: None,
            })
            .collect())
    }

    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, DatabricksError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let keys = self.primary_key(catalog, inner, relation).await?;
        let answer = self
            .ask(&format!(
                "SELECT column_name, full_data_type, is_nullable, column_default \
                 FROM {}.information_schema.columns \
                 WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                quote(catalog),
                literal(inner),
                literal(relation)
            ))
            .await?;
        let name = answer.at("column_name");
        let data_type = answer.at("full_data_type");
        let nullable = answer.at("is_nullable");
        let default = answer.at("column_default");

        Ok(answer
            .rows()
            .iter()
            .enumerate()
            .map(|(at, row)| {
                let column = answer.text(row, name);
                ColumnInfo {
                    is_primary_key: keys.contains(&column),
                    // `full_data_type` and not `data_type`, which is the same
                    // column truncated: a `decimal(18,2)` is `DECIMAL` in one and
                    // `decimal(18,2)` in the other, and an `array<string>` is
                    // `ARRAY`. The trait asks for the type as the database states
                    // it, and only one of the two does.
                    data_type: answer.text(row, data_type),
                    // The row's place in the order the statement asked for; see
                    // the module comment.
                    position: at as i32 + 1,
                    nullable: answer.text(row, nullable) == "YES",
                    default_value: match answer.text(row, default) {
                        empty if empty.is_empty() => None,
                        value => Some(value),
                    },
                    // Unity Catalog states a generated column's expression in
                    // `generation_expression`, a column this query does not ask
                    // for — and asking for it would be writing the one thing no
                    // workspace has confirmed is there. So a generated column
                    // arrives as a column with a default, which is the shape
                    // this field exists to correct; correcting it needs a server.
                    computed: None,
                    name: column,
                }
            })
            .collect())
    }

    /// The statement a view is defined by; `None` for anything else.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, DatabricksError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(None);
        };
        let answer = self
            .ask(&format!(
                "SELECT view_definition FROM {}.information_schema.views \
                 WHERE table_schema = {} AND table_name = {}",
                quote(catalog),
                literal(inner),
                literal(relation)
            ))
            .await?;
        let body = answer.at("view_definition");
        Ok(answer
            .rows()
            .first()
            .map(|row| answer.text(row, body))
            .filter(|text| !text.is_empty()))
    }

    /// Empty, always, and without asking. See the module comment: a Delta table
    /// has file statistics and clustering, and neither is an object beside it.
    pub async fn indexes(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<IndexInfo>, DatabricksError> {
        Ok(Vec::new())
    }

    /// The foreign keys this relation declares.
    ///
    /// Declared and not enforced: Unity Catalog takes `FOREIGN KEY` as
    /// information for the optimizer and for tools, and does not check it on
    /// write. That is worth knowing and is not this driver's to hide — a key the
    /// user declared is a key the structure pane should show.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, DatabricksError> {
        self.relationships(schema, relation, Side::Referencing)
            .await
    }

    /// The foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, DatabricksError> {
        self.relationships(schema, relation, Side::Referenced).await
    }

    /// One end or the other of the same join.
    ///
    /// The two questions are the same statement with the restriction moved from
    /// one side of it to the other, so they are one function: an answer that
    /// disagreed with itself about which columns pair up would be worse than
    /// either being missing.
    async fn relationships(
        &self,
        schema: &str,
        relation: &str,
        side: Side,
    ) -> Result<Vec<RelationshipInfo>, DatabricksError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let catalog_name = quote(catalog);
        let (mine, theirs) = side.aliases();
        let answer = self
            .ask(&format!(
                "SELECT rc.constraint_name AS constraint_name, \
                 {mine}.column_name AS local_column, \
                 {theirs}.table_schema AS other_schema, \
                 {theirs}.table_name AS other_table, \
                 {theirs}.column_name AS other_column, \
                 fk.ordinal_position AS key_sequence, \
                 rc.update_rule AS update_rule, rc.delete_rule AS delete_rule \
                 FROM {catalog_name}.information_schema.referential_constraints rc \
                 JOIN {catalog_name}.information_schema.key_column_usage fk \
                 ON fk.constraint_catalog = rc.constraint_catalog \
                 AND fk.constraint_schema = rc.constraint_schema \
                 AND fk.constraint_name = rc.constraint_name \
                 JOIN {catalog_name}.information_schema.key_column_usage pk \
                 ON pk.constraint_catalog = rc.unique_constraint_catalog \
                 AND pk.constraint_schema = rc.unique_constraint_schema \
                 AND pk.constraint_name = rc.unique_constraint_name \
                 AND pk.ordinal_position = fk.position_in_unique_constraint \
                 WHERE {mine}.table_schema = {} AND {mine}.table_name = {} \
                 ORDER BY constraint_name, key_sequence",
                literal(inner),
                literal(relation)
            ))
            .await?;

        let name = answer.at("constraint_name");
        let local = answer.at("local_column");
        let other_schema = answer.at("other_schema");
        let other_table = answer.at("other_table");
        let other_column = answer.at("other_column");
        let on_update = answer.at("update_rule");
        let on_delete = answer.at("delete_rule");

        // One row per column pair, already in key order, so a constraint is
        // gathered by appending to whichever entry has its name.
        let mut found: Vec<RelationshipInfo> = Vec::new();
        for row in answer.rows() {
            let constraint = answer.text(row, name);
            if found.last().map(|held| held.name.as_str()) != Some(constraint.as_str()) {
                found.push(RelationshipInfo {
                    name: constraint,
                    local_columns: Vec::new(),
                    // The schema as the navigator spells one, which is
                    // `catalog.schema` — a foreign key inside one catalog stays
                    // inside it, since Unity Catalog has no cross-catalog
                    // constraints.
                    other_schema: format!("{catalog}.{}", answer.text(row, other_schema)),
                    other_table: answer.text(row, other_table),
                    other_columns: Vec::new(),
                    on_update: answer.text(row, on_update),
                    on_delete: answer.text(row, on_delete),
                });
            }
            let held = found.last_mut().expect("one was just pushed");
            held.local_columns.push(answer.text(row, local));
            held.other_columns.push(answer.text(row, other_column));
        }
        Ok(found)
    }

    /// The check constraints on this relation.
    ///
    /// Check only. Primary and foreign keys have calls of their own above, and
    /// `NOT NULL` is on the column where it belongs rather than synthesised here
    /// as an object nobody could drop. A `CHECK` in Unity Catalog *is* enforced,
    /// unlike the keys, which is worth remembering when the structure pane shows
    /// both in the same list.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, DatabricksError> {
        let Some((catalog, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let catalog_name = quote(catalog);
        let answer = self
            .ask(&format!(
                "SELECT cc.constraint_name AS constraint_name, cc.check_clause AS check_clause \
                 FROM {catalog_name}.information_schema.check_constraints cc \
                 JOIN {catalog_name}.information_schema.table_constraints tc \
                 ON tc.constraint_catalog = cc.constraint_catalog \
                 AND tc.constraint_schema = cc.constraint_schema \
                 AND tc.constraint_name = cc.constraint_name \
                 WHERE tc.table_schema = {} AND tc.table_name = {} \
                 ORDER BY constraint_name",
                literal(inner),
                literal(relation)
            ))
            .await?;
        let name = answer.at("constraint_name");
        let clause = answer.at("check_clause");
        Ok(answer
            .rows()
            .iter()
            .map(|row| ConstraintInfo {
                name: answer.text(row, name),
                kind: ConstraintKind::Check,
                // The clause as the catalog holds it, with `CHECK` around it —
                // which is what the user typed and what `SHOW TBLPROPERTIES`
                // would give back.
                definition: format!("CHECK ({})", answer.text(row, clause)),
            })
            .collect())
    }

    /// Empty, always, and without asking. See the module comment.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, DatabricksError> {
        Ok(Vec::new())
    }

    /// The columns of this relation's primary key, in key order.
    async fn primary_key(
        &self,
        catalog: &str,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<String>, DatabricksError> {
        let catalog_name = quote(catalog);
        let answer = self
            .ask(&format!(
                "SELECT kcu.column_name AS column_name \
                 FROM {catalog_name}.information_schema.table_constraints tc \
                 JOIN {catalog_name}.information_schema.key_column_usage kcu \
                 ON kcu.constraint_catalog = tc.constraint_catalog \
                 AND kcu.constraint_schema = tc.constraint_schema \
                 AND kcu.constraint_name = tc.constraint_name \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                 AND tc.table_schema = {} AND tc.table_name = {} \
                 ORDER BY kcu.ordinal_position",
                literal(schema),
                literal(relation)
            ))
            .await?;
        let column = answer.at("column_name");
        Ok(answer
            .rows()
            .iter()
            .map(|row| answer.text(row, column))
            .collect())
    }
}

/// Which end of a foreign key the relation being asked about is.
#[derive(Clone, Copy)]
enum Side {
    /// The table that declares the key: its own columns are the `fk` ones.
    Referencing,
    /// The table the key points at: its own columns are the `pk` ones.
    Referenced,
}

impl Side {
    /// The alias for this relation's own columns, and for the other end's.
    fn aliases(self) -> (&'static str, &'static str) {
        match self {
            Side::Referencing => ("fk", "pk"),
            Side::Referenced => ("pk", "fk"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view is not a table, a federated table is somewhere else, and a
    /// streaming table is said to be unrecognised rather than given a table's
    /// actions.
    #[test]
    fn only_the_kinds_unity_catalog_documents_are_claimed() {
        assert_eq!(relation_kind("MANAGED"), RelationKind::Table);
        assert_eq!(relation_kind("EXTERNAL"), RelationKind::Table);
        assert_eq!(relation_kind("VIEW"), RelationKind::View);
        assert_eq!(
            relation_kind("MATERIALIZED_VIEW"),
            RelationKind::MaterializedView
        );
        assert_eq!(relation_kind("FOREIGN"), RelationKind::ForeignTable);
        assert_eq!(relation_kind("STREAMING_TABLE"), RelationKind::Unknown);
    }

    /// The two questions are one statement with the restriction on the other
    /// side, and swapping the pair is the whole difference between them.
    #[test]
    fn the_two_ends_of_a_foreign_key_swap_which_columns_are_local() {
        assert_eq!(Side::Referencing.aliases(), ("fk", "pk"));
        assert_eq!(Side::Referenced.aliases(), ("pk", "fk"));
    }
}
