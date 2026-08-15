//! What Snowflake can say about itself, in the shape the navigator expects.
//!
//! Two kinds of statement, and the split is not tidiness. `INFORMATION_SCHEMA`
//! is a per-database set of views and answers the questions the SQL standard has
//! a place for — tables, columns, view bodies. Everything about keys is a `SHOW`
//! command instead, because Snowflake's `INFORMATION_SCHEMA` has
//! `TABLE_CONSTRAINTS` and *not* `KEY_COLUMN_USAGE`, so the constraint is
//! nameable there and its columns are not. `SHOW PRIMARY KEYS`, `SHOW IMPORTED
//! KEYS`, `SHOW EXPORTED KEYS` and `SHOW UNIQUE KEYS` each answer with one row
//! per column and a `key_sequence` to put them in order, which is the shape the
//! trait wants.
//!
//! **A `SHOW` answer is read by column name and never by position.** Snowflake
//! documents the columns of each and has added to them between releases; a
//! driver counting from the left would silently start reading a neighbouring
//! column. `Catalog::at` is that lookup, and a column that is not there reads as
//! empty rather than failing — an empty name is visible in the navigator, where
//! a failed refresh is not.
//!
//! **Two calls answer with nothing and send no statement to find out.**
//!
//! - There are no indexes. Snowflake stores tables as micro-partitions with
//!   automatic metadata, and the nearest thing to an index — a clustering key —
//!   is a property of the table rather than an object beside it. `CREATE INDEX`
//!   is not in the grammar and there is no catalog view for one, so a hybrid
//!   table's secondary indexes, which do exist, would need a different question
//!   altogether and are not asked for here.
//! - There are no triggers. Snowflake's answer to the same need is a stream and
//!   a task, which are scheduled objects rather than something attached to a
//!   table, and reporting them here would put two unrelated things in a section
//!   labelled for a third.
//!
//! Nothing in this file has been run. Every statement is transcribed from
//! Snowflake's published catalog documentation, and the column names each one is
//! read by are the specific thing a first real connection would either confirm
//! or make a liar of.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo,
};

use crate::{SnowflakeError, SnowflakeSource, literal, parts, quote};

/// `INFORMATION_SCHEMA.TABLES.TABLE_TYPE`, as one of the kinds the navigator
/// knows about.
///
/// An external table is a definition over files somebody else owns, which is
/// what `ForeignTable` means everywhere else in this trait — the storage is not
/// the database's. `Unknown` for anything else says what is true, which is that
/// this driver has not met it, rather than offering a table's actions for
/// something that is not one.
fn relation_kind(table_type: &str) -> RelationKind {
    match table_type {
        "BASE TABLE" | "TEMPORARY TABLE" => RelationKind::Table,
        "VIEW" => RelationKind::View,
        "MATERIALIZED VIEW" => RelationKind::MaterializedView,
        "EXTERNAL TABLE" => RelationKind::ForeignTable,
        _ => RelationKind::Unknown,
    }
}

/// One foreign key while its columns are still arriving.
///
/// A `SHOW … KEYS` answer is one row per column, so a two-column key is two rows
/// that have to find each other and then be put in `key_sequence` order. This
/// holds the half-built relationship and the pairs waiting to be sorted into it.
struct Gathering {
    info: RelationshipInfo,
    /// `(key_sequence, this side's column, the other side's column)`.
    columns: Vec<(i64, String, String)>,
}

/// The three-part name a `SHOW … IN TABLE` needs, every part quoted.
fn qualified(database: &str, schema: &str, relation: &str) -> String {
    format!("{}.{}.{}", quote(database), quote(schema), quote(relation))
}

impl SnowflakeSource {
    /// Every schema in the account, as `database.schema`.
    ///
    /// One statement rather than a `SHOW DATABASES` followed by one
    /// `INFORMATION_SCHEMA.SCHEMATA` per database — which would be a round trip
    /// per database, and `INFORMATION_SCHEMA` is per-database precisely so that
    /// it cannot answer for the account. `SHOW SCHEMAS IN ACCOUNT` is the one
    /// question that covers everything the role can see, which is the same
    /// reasoning the Trino driver gives for reaching `system.jdbc.schemas`.
    ///
    /// Nothing is hidden, `INFORMATION_SCHEMA` included: it is where every other
    /// question on this page is answered, and a navigator that hid it would be
    /// hiding the views it is built on.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, SnowflakeError> {
        let answer = self.ask("SHOW SCHEMAS IN ACCOUNT").await?;
        let database = answer.at("database_name");
        let name = answer.at("name");
        let mut schemas: Vec<SchemaInfo> = answer
            .rows()
            .iter()
            .map(|row| SchemaInfo {
                name: format!("{}.{}", answer.text(row, database), answer.text(row, name)),
            })
            .collect();
        // `SHOW` has an `ORDER BY` of its own only through a `RESULT_SCAN`, which
        // is a second statement to buy an ordering that costs nothing here.
        schemas.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(schemas)
    }

    /// The tables and views in one `database.schema`.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, SnowflakeError> {
        let Some((database, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let answer = self
            .ask(&format!(
                "SELECT TABLE_NAME, TABLE_TYPE, ROW_COUNT FROM {}.INFORMATION_SCHEMA.TABLES \
                 WHERE TABLE_SCHEMA = {} ORDER BY TABLE_NAME",
                quote(database),
                literal(inner)
            ))
            .await?;
        let name = answer.at("TABLE_NAME");
        let kind = answer.at("TABLE_TYPE");
        let rows = answer.at("ROW_COUNT");
        Ok(answer
            .rows()
            .iter()
            .map(|row| RelationInfo {
                schema: schema.to_string(),
                name: answer.text(row, name),
                kind: relation_kind(&answer.text(row, kind)),
                // Snowflake keeps a maintained row count on the table itself, so
                // unlike Trino this costs nothing to report and needs no scan.
                // It is null for a view, which is what `None` is for.
                estimated_rows: answer.text(row, rows).parse().ok(),
            })
            .collect())
    }

    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, SnowflakeError> {
        let Some((database, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let keys = self.primary_key(database, inner, relation).await?;
        let answer = self
            .ask(&format!(
                "SELECT COLUMN_NAME, DATA_TYPE, ORDINAL_POSITION, IS_NULLABLE, COLUMN_DEFAULT, \
                 NUMERIC_PRECISION, NUMERIC_SCALE, CHARACTER_MAXIMUM_LENGTH \
                 FROM {}.INFORMATION_SCHEMA.COLUMNS \
                 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION",
                quote(database),
                literal(inner),
                literal(relation)
            ))
            .await?;

        let name = answer.at("COLUMN_NAME");
        let data_type = answer.at("DATA_TYPE");
        let position = answer.at("ORDINAL_POSITION");
        let nullable = answer.at("IS_NULLABLE");
        let default = answer.at("COLUMN_DEFAULT");
        let precision = answer.at("NUMERIC_PRECISION");
        let scale = answer.at("NUMERIC_SCALE");
        let length = answer.at("CHARACTER_MAXIMUM_LENGTH");

        Ok(answer
            .rows()
            .iter()
            .map(|row| {
                let column = answer.text(row, name);
                ColumnInfo {
                    is_primary_key: keys.contains(&column),
                    // Composed, because Snowflake's catalog has no column holding
                    // the declared type as a person wrote it: `DATA_TYPE` says
                    // `NUMBER` and the precision is three columns away. What is
                    // put back together here is what `DESC TABLE` would show.
                    data_type: declared(
                        &answer.text(row, data_type),
                        &answer.text(row, precision),
                        &answer.text(row, scale),
                        &answer.text(row, length),
                    ),
                    // Already one-based in this catalog, so nothing to convert.
                    position: answer.text(row, position).parse().unwrap_or_default(),
                    nullable: answer.text(row, nullable) == "YES",
                    default_value: match answer.text(row, default) {
                        empty if empty.is_empty() => None,
                        value => Some(value),
                    },
                    // `INFORMATION_SCHEMA.COLUMNS` has no column that says a
                    // value is derived rather than defaulted, so a Snowflake
                    // virtual column would arrive here as a default. Left as a
                    // plain default rather than guessed at from the expression's
                    // shape, which is one more thing that would need a server to
                    // settle.
                    computed: None,
                    name: column,
                }
            })
            .collect())
    }

    /// The statement a view is defined by; `None` for anything else.
    ///
    /// The body as `INFORMATION_SCHEMA.VIEWS` holds it, which for Snowflake is
    /// the whole `CREATE VIEW` statement rather than the query alone — that is
    /// what this database keeps, and the trait asks for the definition as the
    /// database holds it.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, SnowflakeError> {
        let Some((database, inner)) = parts(schema) else {
            return Ok(None);
        };
        let answer = self
            .ask(&format!(
                "SELECT VIEW_DEFINITION FROM {}.INFORMATION_SCHEMA.VIEWS \
                 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
                quote(database),
                literal(inner),
                literal(relation)
            ))
            .await?;
        let body = answer.at("VIEW_DEFINITION");
        Ok(answer
            .rows()
            .first()
            .map(|row| answer.text(row, body))
            .filter(|text| !text.is_empty()))
    }

    /// Empty, always, and without asking. See the module comment: Snowflake has
    /// no `CREATE INDEX` and no catalog view for one.
    pub async fn indexes(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<IndexInfo>, SnowflakeError> {
        Ok(Vec::new())
    }

    /// The foreign keys this relation declares.
    ///
    /// Declared, and not enforced: Snowflake takes `FOREIGN KEY` and does not
    /// check it, using it only to plan. That is worth knowing and is not this
    /// driver's to say — a key the user declared is a key the structure pane
    /// should show, whether or not the database defends it.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, SnowflakeError> {
        self.relationships(schema, relation, "IMPORTED").await
    }

    /// The foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, SnowflakeError> {
        self.relationships(schema, relation, "EXPORTED").await
    }

    /// One end or the other of the same `SHOW` answer.
    ///
    /// Both commands answer with identical columns, describing the same
    /// constraint from opposite sides: `IMPORTED` lists the keys this table
    /// declares, so its own columns are the `fk_` ones, and `EXPORTED` lists the
    /// keys pointing at it, so its own columns are the `pk_` ones. Which pair is
    /// "local" is therefore the whole difference, and reading the two answers
    /// with one function is what keeps them from drifting apart.
    async fn relationships(
        &self,
        schema: &str,
        relation: &str,
        direction: &str,
    ) -> Result<Vec<RelationshipInfo>, SnowflakeError> {
        let Some((database, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let answer = self
            .ask(&format!(
                "SHOW {direction} KEYS IN TABLE {}",
                qualified(database, inner, relation)
            ))
            .await?;

        let (mine, theirs) = match direction {
            "IMPORTED" => ("fk", "pk"),
            _ => ("pk", "fk"),
        };
        let name = answer.at("fk_name");
        let local = answer.at(&format!("{mine}_column_name"));
        let other_schema = answer.at(&format!("{theirs}_schema_name"));
        let other_table = answer.at(&format!("{theirs}_table_name"));
        let other_column = answer.at(&format!("{theirs}_column_name"));
        let sequence = answer.at("key_sequence");
        let on_update = answer.at("update_rule");
        let on_delete = answer.at("delete_rule");

        // One row per column, so the rows of one constraint have to be gathered
        // and put in `key_sequence` order — a two-column key whose columns
        // arrived the other way round is a relationship pointing at the wrong
        // pair.
        let mut found: Vec<Gathering> = Vec::new();
        for row in answer.rows() {
            let constraint = answer.text(row, name);
            let at = match found.iter().position(|held| held.info.name == constraint) {
                Some(at) => at,
                None => {
                    found.push(Gathering {
                        info: RelationshipInfo {
                            name: constraint,
                            local_columns: Vec::new(),
                            other_schema: format!("{database}.{}", answer.text(row, other_schema)),
                            other_table: answer.text(row, other_table),
                            other_columns: Vec::new(),
                            on_update: answer.text(row, on_update),
                            on_delete: answer.text(row, on_delete),
                        },
                        columns: Vec::new(),
                    });
                    found.len() - 1
                }
            };
            found[at].columns.push((
                answer.text(row, sequence).parse().unwrap_or_default(),
                answer.text(row, local),
                answer.text(row, other_column),
            ));
        }

        Ok(found
            .into_iter()
            .map(|mut gathering| {
                gathering.columns.sort_by_key(|(sequence, _, _)| *sequence);
                for (_, mine, theirs) in gathering.columns {
                    gathering.info.local_columns.push(mine);
                    gathering.info.other_columns.push(theirs);
                }
                gathering.info
            })
            .collect())
    }

    /// The unique constraints on this relation.
    ///
    /// Unique only. Snowflake has no `CHECK` constraint at all, primary and
    /// foreign keys have calls of their own above, and `NOT NULL` is on the
    /// column where it belongs rather than synthesised here as an object nobody
    /// could drop.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, SnowflakeError> {
        let Some((database, inner)) = parts(schema) else {
            return Ok(Vec::new());
        };
        let answer = self
            .ask(&format!(
                "SHOW UNIQUE KEYS IN TABLE {}",
                qualified(database, inner, relation)
            ))
            .await?;
        let name = answer.at("constraint_name");
        let column = answer.at("column_name");
        let sequence = answer.at("key_sequence");

        let mut found: Vec<(String, Vec<(i64, String)>)> = Vec::new();
        for row in answer.rows() {
            let constraint = answer.text(row, name);
            let at = match found.iter().position(|(held, _)| *held == constraint) {
                Some(at) => at,
                None => {
                    found.push((constraint, Vec::new()));
                    found.len() - 1
                }
            };
            found[at].1.push((
                answer.text(row, sequence).parse().unwrap_or_default(),
                answer.text(row, column),
            ));
        }

        Ok(found
            .into_iter()
            .map(|(name, mut columns)| {
                columns.sort_by_key(|(sequence, _)| *sequence);
                let columns: Vec<String> = columns
                    .into_iter()
                    .map(|(_, column)| quote(&column))
                    .collect();
                ConstraintInfo {
                    name,
                    kind: ConstraintKind::Unique,
                    // Composed, because a `SHOW` answers with columns and not
                    // with the text the constraint was written as. `UNIQUE (…)`
                    // is what a person would have typed and what `DESC` shows.
                    definition: format!("UNIQUE ({})", columns.join(", ")),
                }
            })
            .collect())
    }

    /// Empty, always, and without asking. See the module comment: Snowflake's
    /// answer to a trigger is a stream and a task, which are not attached to a
    /// table.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, SnowflakeError> {
        Ok(Vec::new())
    }

    /// The columns of this relation's primary key, in key order.
    async fn primary_key(
        &self,
        database: &str,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<String>, SnowflakeError> {
        let answer = self
            .ask(&format!(
                "SHOW PRIMARY KEYS IN TABLE {}",
                qualified(database, schema, relation)
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

/// A column's type as it was declared, rebuilt from the catalog's pieces.
///
/// `NUMBER(18,2)`, `TEXT(64)`, `TIMESTAMP_NTZ`. The pieces are separate columns
/// in `INFORMATION_SCHEMA.COLUMNS` and only some of them apply to any given
/// type, which is why this is a function with tests rather than a `format!` at
/// the call site: a `TEXT` has a length and no precision, a `NUMBER` has both
/// precision and scale, and a `BOOLEAN` has none of the three.
fn declared(data_type: &str, precision: &str, scale: &str, length: &str) -> String {
    if !precision.is_empty() {
        return match scale {
            "" | "0" => format!("{data_type}({precision})"),
            scale => format!("{data_type}({precision},{scale})"),
        };
    }
    if !length.is_empty() {
        return format!("{data_type}({length})");
    }
    data_type.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view is not a table, an external table is not stored here, and a fourth
    /// kind is said to be unrecognised rather than guessed at.
    #[test]
    fn only_the_kinds_snowflake_documents_are_claimed() {
        assert_eq!(relation_kind("BASE TABLE"), RelationKind::Table);
        assert_eq!(relation_kind("VIEW"), RelationKind::View);
        assert_eq!(
            relation_kind("MATERIALIZED VIEW"),
            RelationKind::MaterializedView
        );
        assert_eq!(relation_kind("EXTERNAL TABLE"), RelationKind::ForeignTable);
        assert_eq!(relation_kind("HYBRID TABLE"), RelationKind::Unknown);
    }

    /// The declared type is three catalog columns put back together, and each
    /// type uses a different subset of them.
    #[test]
    fn a_declared_type_uses_only_the_pieces_its_type_has() {
        assert_eq!(declared("NUMBER", "18", "2", ""), "NUMBER(18,2)");
        assert_eq!(declared("NUMBER", "38", "0", ""), "NUMBER(38)");
        assert_eq!(declared("TEXT", "", "", "64"), "TEXT(64)");
        assert_eq!(declared("BOOLEAN", "", "", ""), "BOOLEAN");
        assert_eq!(declared("TIMESTAMP_NTZ", "", "", ""), "TIMESTAMP_NTZ");
    }

    /// Every part of a three-level name is quoted, because Snowflake folds an
    /// unquoted one up and the name came from a catalog that already knows its
    /// case.
    #[test]
    fn a_show_command_names_all_three_levels_and_quotes_each() {
        assert_eq!(
            qualified("SALES", "PUBLIC", "orders"),
            r#""SALES"."PUBLIC"."orders""#
        );
    }
}
