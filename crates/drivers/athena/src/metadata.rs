//! What Athena can say about itself, asked through the catalog actions rather
//! than in SQL.
//!
//! **No server has answered any of this.** The three actions and their shapes
//! are read from the Athena API reference.
//!
//! **Nothing here runs a query, and that is the decision this file is about.**
//! `SHOW DATABASES`, `SHOW TABLES` and `DESCRIBE` all work in Athena and every
//! one of them is a **query execution**: queued, scanned, billed, and written to
//! S3 as a result file that then has to be read back. `ListDatabases`,
//! `ListTableMetadata` and `GetTableMetadata` answer the same questions as
//! ordinary API calls that cost nothing and return in one round trip. A
//! navigator that expanded a tree by running SQL would charge somebody for
//! opening it — and would leave a trail of result files behind in the bucket.
//!
//! **What answers here is Glue and not Athena**, which is why five of the nine
//! calls are empty and why the vocabulary changes. Athena has no catalog of its
//! own: a table is a Hive table definition in the AWS Glue Data Catalog, and
//! these three actions are a thin wrapper over Glue's. So:
//!
//! - **The types are Hive's** — `int`, `bigint`, `string`, `struct<a:int>` —
//!   where the same columns in a *result* are described in Presto's:
//!   `integer`, `varchar`, `row`. Both are true and they describe different
//!   things: this one is what the table was declared as, and `arrow_map.rs`
//!   reads the other because that is what a value arrives as. The Trino driver
//!   has exactly this split between `information_schema.columns` and a result's
//!   `typeSignature`.
//! - **There are no keys.** Hive declares no primary key and no foreign key, so
//!   `is_primary_key` is false for every column of every table this driver will
//!   report, and both relationship calls are empty. Nothing is hidden by that:
//!   there is nothing to hide.
//! - **There are no indexes.** A Hive table is a prefix in a bucket and a list
//!   of partitions; what stands in for an index is partitioning and file-level
//!   statistics, and neither is an `IndexInfo`. Reporting partitions as indexes
//!   was considered and rejected — a partition is not something a planner can
//!   use *instead of* a scan, it is how the scan is narrowed, and a structure
//!   pane that called it an index would be inviting somebody to drop it.
//! - **There are no constraints and no triggers.** Hive has neither, and there
//!   is no syntax in Athena for either.
//!
//! **Nullability is not asked and not claimed.** Glue records no nullability
//! for a column, so every column here is reported nullable — which is the true
//! statement about a Hive table, where a file simply may not have a value. A
//! driver that reported `NOT NULL` because the data happens to be complete
//! would be describing today's files rather than the table.

use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    SchemaInfo, TriggerInfo,
};

use crate::wire::{Column, DatabaseList, SingleTableMetadata, TableMetadata, TableMetadataList};
use crate::{AthenaError, AthenaSource};

/// `TableMetadata.TableType` as one of the kinds a navigator draws an icon for.
///
/// `EXTERNAL_TABLE` becomes `Table` and not `ForeignTable`, which is the one
/// judgement call here. In Hive's vocabulary every Athena table is external,
/// because the data is in S3 and Athena owns none of it — so mapping the word
/// to `ForeignTable` would be literally true and would draw *every* table in
/// the navigator as foreign, which is a distinction that distinguishes nothing.
/// `MANAGED_TABLE`, which is what an Iceberg table Athena created reports, is
/// the same kind of thing from the other direction.
fn relation_kind(table_type: &str) -> RelationKind {
    match table_type {
        "EXTERNAL_TABLE" | "MANAGED_TABLE" => RelationKind::Table,
        "VIRTUAL_VIEW" => RelationKind::View,
        _ => RelationKind::Unknown,
    }
}

/// The Hive statistic for a table's size, where somebody has written one.
///
/// `numRows` is a table property rather than something Glue measures: a crawler
/// writes it, `ANALYZE TABLE` writes it, and a table nobody has run either
/// against has no such property. `None` is therefore the honest answer far more
/// often here than in a database that keeps its own statistics — and it means
/// what the field says it means, which is that nothing has measured this.
fn estimated_rows(table: &TableMetadata) -> Option<i64> {
    table.parameters.get("numRows")?.parse().ok()
}

/// The columns of a table, in the order a row arrives in.
///
/// The partition keys come last, because that is where Hive puts them: they are
/// kept in a separate list in the catalog and appear after the ordinary columns
/// in `SELECT *`. Leaving them out was the alternative and it would mean a
/// structure pane showing fewer columns than the grid does.
fn all_columns(table: &TableMetadata) -> Vec<Column> {
    table
        .columns
        .iter()
        .chain(table.partition_keys.iter())
        .cloned()
        .collect()
}

impl AthenaSource {
    /// The databases in this connection's data catalog.
    ///
    /// Bare names and not `catalog.database`, which is where this driver parts
    /// company with the Trino and Flight SQL ones — a connection names one
    /// catalog and every database listed is in it, so there is no second level
    /// for the name to carry. Which catalog is a connection-string question, for
    /// the reason `AthenaSource::catalog` gives.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, AthenaError> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let mut request = serde_json::json!({ "CatalogName": self.catalog() });
            if !token.is_empty() {
                request["NextToken"] = serde_json::json!(token);
            }
            let listing: DatabaseList = self.wire().call("ListDatabases", request).await?;
            out.extend(
                listing
                    .database_list
                    .into_iter()
                    .filter(|database| !database.name.is_empty())
                    .map(|database| SchemaInfo {
                        name: database.name,
                    }),
            );
            if listing.next_token.is_empty() {
                return Ok(out);
            }
            token = listing.next_token;
        }
    }

    /// The tables and views in one database.
    ///
    /// `ListTableMetadata` answers with the whole of each table — columns,
    /// partitions and properties — rather than with a list of names, which is
    /// why `estimated_rows` can be filled here without a second call per table.
    /// That is one better than the BigQuery driver manages against its own
    /// listing endpoint, and it is the API's doing rather than this driver's.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, AthenaError> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let mut request = serde_json::json!({
                "CatalogName": self.catalog(),
                "DatabaseName": schema,
            });
            if !token.is_empty() {
                request["NextToken"] = serde_json::json!(token);
            }
            let listing: TableMetadataList = self.wire().call("ListTableMetadata", request).await?;
            for table in &listing.table_metadata_list {
                out.push(RelationInfo {
                    schema: schema.to_string(),
                    name: table.name.clone(),
                    kind: relation_kind(&table.table_type),
                    estimated_rows: estimated_rows(table),
                });
            }
            if listing.next_token.is_empty() {
                return Ok(out);
            }
            token = listing.next_token;
        }
    }

    /// The columns of one relation, as the Glue catalog declares them.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, AthenaError> {
        let table = self.table(schema, relation).await?;
        Ok(all_columns(&table)
            .into_iter()
            .enumerate()
            .map(|(at, column)| ColumnInfo {
                name: column.name,
                // Hive's spelling, which is what the table was declared with and
                // what `SHOW CREATE TABLE` would print. Not Presto's, which is
                // what `arrow_map` reads off a result — see the module comment.
                data_type: column.r#type,
                // Glue records no nullability; every column of a Hive table may
                // be missing from a file.
                nullable: true,
                position: at as i32 + 1,
                // False for every column of every table: Hive declares no keys.
                is_primary_key: false,
                // And no defaults. A Hive column has no default expression —
                // the value in the file is the value.
                default_value: None,
                // Which also settles this one: with no expression in the field
                // beside it, there is nothing for the flag to disambiguate.
                computed: None,
            })
            .collect())
    }

    /// Always `None`, and without asking a second time.
    ///
    /// Athena's view definitions are real and this API does not carry them.
    /// `GetTableMetadata` answers with the columns and the Hive properties; the
    /// text of a Presto view lives in the Glue table's `ViewOriginalText`, which
    /// this action does not return — and where it does exist it is a base64
    /// blob wrapped in `/* Presto View: … */`, not SQL somebody could read.
    /// `SHOW CREATE VIEW` gives the readable form and it is a query execution,
    /// which is what this file exists to avoid for a navigator click.
    ///
    /// So a view here is listed as a view and its text is not shown. That is a
    /// gap, and the fix is a second AWS service — Glue's own `GetTable` — rather
    /// than anything Athena can be asked.
    pub async fn definition(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Option<String>, AthenaError> {
        Ok(None)
    }

    /// Empty, always, and without asking. A Hive table has no index: what stands
    /// in for one is partitioning, which is not an `IndexInfo` and would invite
    /// somebody to drop it if it were listed as one.
    pub async fn indexes(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<IndexInfo>, AthenaError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Hive declares no foreign keys, because
    /// it has no primary keys for one to reference.
    pub async fn foreign_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, AthenaError> {
        Ok(Vec::new())
    }

    /// Empty for the same reason as `foreign_keys`, from the other end.
    pub async fn referenced_by(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, AthenaError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Hive has no constraint syntax and Glue
    /// has no constraint catalog; there is not even a nullability to report.
    pub async fn constraints(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<ConstraintInfo>, AthenaError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. Athena has no triggers.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, AthenaError> {
        Ok(Vec::new())
    }

    /// One table's whole definition.
    async fn table(&self, schema: &str, relation: &str) -> Result<TableMetadata, AthenaError> {
        let answer: SingleTableMetadata = self
            .wire()
            .call(
                "GetTableMetadata",
                serde_json::json!({
                    "CatalogName": self.catalog(),
                    "DatabaseName": schema,
                    "TableName": relation,
                }),
            )
            .await?;
        Ok(answer.table_metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn column(name: &str, kind: &str) -> Column {
        Column {
            name: name.to_string(),
            r#type: kind.to_string(),
        }
    }

    /// The judgement call in this file, pinned: every Athena table is external
    /// in Hive's vocabulary, and drawing them all as foreign tables would be a
    /// distinction that distinguishes nothing.
    #[test]
    fn every_athena_table_is_a_table_and_not_a_foreign_one() {
        assert_eq!(relation_kind("EXTERNAL_TABLE"), RelationKind::Table);
        assert_eq!(relation_kind("MANAGED_TABLE"), RelationKind::Table);
        assert_eq!(relation_kind("VIRTUAL_VIEW"), RelationKind::View);
        // A kind this driver has not met is said to be unrecognised rather than
        // drawn as a table somebody could write to.
        assert_eq!(relation_kind("GOVERNED"), RelationKind::Unknown);
    }

    /// The partition columns are columns, and they come last because that is
    /// where a row puts them. A structure pane that left them out would show
    /// fewer columns than the grid.
    #[test]
    fn the_partition_keys_are_columns_and_they_come_last() {
        let table = TableMetadata {
            name: "events".to_string(),
            table_type: "EXTERNAL_TABLE".to_string(),
            columns: vec![column("id", "bigint"), column("body", "string")],
            partition_keys: vec![column("dt", "string")],
            parameters: HashMap::new(),
        };
        let names: Vec<String> = all_columns(&table)
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(names, ["id", "body", "dt"]);
    }

    /// A row count nobody has written is `None` rather than zero — declining to
    /// answer is not the same as answering that the table is empty, and only one
    /// of them is true.
    #[test]
    fn a_row_count_is_the_hive_property_or_nothing() {
        let mut table = TableMetadata::default();
        assert_eq!(estimated_rows(&table), None);

        table
            .parameters
            .insert("numRows".to_string(), "1200".to_string());
        assert_eq!(estimated_rows(&table), Some(1200));

        // Hive properties are free-form text, so one that is not a number is
        // declined rather than read as zero.
        table
            .parameters
            .insert("numRows".to_string(), "unknown".to_string());
        assert_eq!(estimated_rows(&table), None);
    }
}
