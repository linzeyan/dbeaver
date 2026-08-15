//! What BigQuery can say about itself, asked over REST rather than in SQL.
//!
//! **No server has answered any of this.** The endpoints and the field names are
//! read from the BigQuery REST v2 reference.
//!
//! **Nothing here runs a query, and that is the decision this file is about.**
//! BigQuery has an `INFORMATION_SCHEMA` and it would answer every question
//! below, and every one of those answers would be a *job*: a scheduled unit of
//! work with a latency measured in seconds and a line on the bill. A navigator
//! expanding a dataset of two hundred tables would submit two hundred jobs. The
//! REST catalog — `datasets.list`, `tables.list`, `tables.get` — answers the
//! same questions as ordinary API calls that cost nothing and return in one
//! round trip, so that is what this file uses, and the four questions it cannot
//! answer that way are answered as empty with the reason on the method.
//!
//! What the REST resource carries that is worth knowing:
//!
//! - **`tables.get` has the whole schema**, including nested fields, modes and
//!   the precision of a `NUMERIC`. So `columns` is one request.
//! - **`tables.get` has `tableConstraints`**, which is where BigQuery's
//!   unenforced primary and foreign keys live. They are declarations the query
//!   planner may use and the storage layer does not enforce — which is worth
//!   saying in a client, and is said by reporting them exactly as the catalog
//!   states them rather than by leaving them out.
//! - **`tables.get` has the view text**, for both a view and a materialized one.
//!
//! What it does not carry, and what each of those costs instead:
//!
//! - **Inbound foreign keys.** A table's own `tableConstraints` name what it
//!   references and nothing names what references it. Finding those means
//!   either a `tables.get` per table in the dataset or a query against
//!   `INFORMATION_SCHEMA.CONSTRAINT_COLUMN_USAGE` — a job, per click.
//! - **Indexes.** BigQuery has search indexes and vector indexes, and they are
//!   listed only in `INFORMATION_SCHEMA.SEARCH_INDEXES` and
//!   `…VECTOR_INDEXES`. Also a job, per click. And they are not the kind of
//!   index a structure pane is describing: there is no `CREATE INDEX` on an
//!   ordinary column, because there is no row storage for one to sort.
//! - **Row counts, in a listing.** `tables.list` does not carry `numRows`;
//!   `tables.get` does, which is one request per table.

use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    SchemaInfo, TriggerInfo,
};

use crate::rest::{DatasetPage, Table, TableField, TablePage};
use crate::{BigQueryError, BigQuerySource, rest};

/// `Table.type` as one of the kinds a navigator draws an icon for.
///
/// `EXTERNAL` becomes `ForeignTable`, which is what the word means everywhere
/// else in this workspace — a relation whose rows are somewhere the database
/// does not own. `SNAPSHOT` and `CLONE` are BigQuery-only shapes with no row in
/// `RelationKind`, and they are `Unknown` rather than shoehorned into `Table`:
/// the difference matters to a structure pane, which would otherwise offer to
/// write to something that cannot be written to.
fn relation_kind(kind: &str) -> RelationKind {
    match kind {
        "TABLE" => RelationKind::Table,
        "VIEW" => RelationKind::View,
        "MATERIALIZED_VIEW" => RelationKind::MaterializedView,
        "EXTERNAL" => RelationKind::ForeignTable,
        _ => RelationKind::Unknown,
    }
}

/// A column's type as GoogleSQL spells it.
///
/// The REST catalog answers in the *legacy* vocabulary — `INTEGER`, `FLOAT`,
/// `BOOLEAN`, `RECORD` — whatever the table was created with, and those four
/// words are not accepted by the SQL the editor beside this pane is writing.
/// `INT64`, `FLOAT64`, `BOOL` and `STRUCT` are. `ColumnInfo::data_type` asks for
/// "the type as the database states it", and BigQuery states it two ways; this
/// is the one a person could type back.
///
/// A repeated field is an `ARRAY<…>` of its own type, because that is what it is
/// in a result: BigQuery has no separate array type declaration, and `mode:
/// REPEATED` is how one is spelled in this catalog.
fn type_name(field: &TableField) -> String {
    let base = match field.r#type.as_str() {
        "INTEGER" => "INT64".to_string(),
        "FLOAT" => "FLOAT64".to_string(),
        "BOOLEAN" => "BOOL".to_string(),
        "RECORD" | "STRUCT" => format!(
            "STRUCT<{}>",
            field
                .fields
                .iter()
                .map(|inner| format!("{} {}", inner.name, type_name(inner)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // The parameterised scalars, whose parameters are separate fields here
        // and part of the type everywhere else.
        "NUMERIC" | "BIGNUMERIC" => parameterised(&field.r#type, &field.precision, &field.scale),
        "STRING" | "BYTES" => match &field.max_length {
            Some(length) if !length.is_empty() => format!("{}({length})", field.r#type),
            _ => field.r#type.clone(),
        },
        other => other.to_string(),
    };
    if field.mode == "REPEATED" {
        return format!("ARRAY<{base}>");
    }
    base
}

/// `NUMERIC(38, 9)` out of the two strings the catalog carries it as.
///
/// A scale with no precision is not a thing BigQuery declares, so it is ignored
/// rather than rendered as `NUMERIC(, 9)`.
fn parameterised(name: &str, precision: &Option<String>, scale: &Option<String>) -> String {
    match (
        precision.as_deref().filter(|p| !p.is_empty()),
        scale.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(precision), Some(scale)) => format!("{name}({precision}, {scale})"),
        (Some(precision), None) => format!("{name}({precision})"),
        _ => name.to_string(),
    }
}

impl BigQuerySource {
    /// The datasets in this connection's project.
    ///
    /// Bare dataset ids and not `project.dataset`, which is where this driver
    /// parts company with the DuckDB, Trino and Flight SQL ones — see the crate
    /// comment. A connection names one project and every dataset here is in it,
    /// so there is no second level for the name to carry.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, BigQueryError> {
        let mut out = Vec::new();
        let mut page = String::new();
        loop {
            let listing: DatasetPage = self
                .api()
                .get(&rest::datasets_url(self.project(), &page))
                .await?;
            out.extend(listing.datasets.into_iter().filter_map(|entry| {
                let name = entry.dataset_reference.dataset_id;
                (!name.is_empty()).then_some(SchemaInfo { name })
            }));
            if listing.next_page_token.is_empty() {
                return Ok(out);
            }
            page = listing.next_page_token;
        }
    }

    /// The tables and views in one dataset.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, BigQueryError> {
        let mut out = Vec::new();
        let mut page = String::new();
        loop {
            let listing: TablePage = self
                .api()
                .get(&rest::tables_url(self.project(), schema, &page))
                .await?;
            for entry in listing.tables {
                out.push(RelationInfo {
                    schema: schema.to_string(),
                    name: entry.table_reference.table_id,
                    kind: relation_kind(&entry.r#type),
                    // Deliberately not filled. `tables.list` does not carry a row
                    // count and `tables.get` does, so filling this would be one
                    // extra request per table — two hundred of them for a
                    // navigator expanding a dataset. `None` means "nothing has
                    // measured this", which is exactly the case until somebody
                    // opens the table.
                    estimated_rows: None,
                });
            }
            if listing.next_page_token.is_empty() {
                return Ok(out);
            }
            page = listing.next_page_token;
        }
    }

    /// The columns of one relation, out of the table resource.
    ///
    /// Top-level fields only. A `STRUCT` column is one column whose type is
    /// written out, rather than one entry per leaf: a result set has one column
    /// called `address` of struct type, and a structure pane that listed
    /// `address.city` beside it would disagree with the grid about how many
    /// columns the table has.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, BigQueryError> {
        let table: Table = self
            .api()
            .get(&rest::table_url(self.project(), schema, relation))
            .await?;
        let key = table
            .table_constraints
            .as_ref()
            .and_then(|c| c.primary_key.as_ref())
            .map(|k| k.columns.clone())
            .unwrap_or_default();

        Ok(table
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(at, field)| ColumnInfo {
                name: field.name.clone(),
                data_type: type_name(field),
                // `REQUIRED` is BigQuery's `NOT NULL`; `REPEATED` is an array,
                // which cannot itself be null — an empty array is the empty
                // case, and there is no third state.
                nullable: field.mode != "REQUIRED" && field.mode != "REPEATED",
                position: at as i32 + 1,
                is_primary_key: key.iter().any(|name| name == &field.name),
                default_value: field
                    .default_value_expression
                    .clone()
                    .filter(|value| !value.is_empty()),
            })
            .collect())
    }

    /// The statement a view is defined by; `None` for anything else.
    ///
    /// Both kinds, because BigQuery keeps them in different fields of the same
    /// resource and a structure pane asking about a materialized view wants the
    /// same thing. The body without the `CREATE VIEW` around it, which is what
    /// the resource holds and what the trait asks for.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, BigQueryError> {
        let table: Table = self
            .api()
            .get(&rest::table_url(self.project(), schema, relation))
            .await?;
        Ok(table
            .view
            .or(table.materialized_view)
            .map(|view| view.query)
            .filter(|query| !query.is_empty()))
    }

    /// Empty, always, and without asking.
    ///
    /// BigQuery has no index a structure pane is describing: there is no
    /// `CREATE INDEX` on an ordinary column, because there is no row store for
    /// one to order. What it does have is search indexes and vector indexes,
    /// which are listed only in `INFORMATION_SCHEMA` — a query job, per
    /// navigator click, for a kind of index that is not what this field means.
    pub async fn indexes(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<IndexInfo>, BigQueryError> {
        Ok(Vec::new())
    }

    /// The foreign keys this relation declares.
    ///
    /// Unenforced, every one of them: BigQuery takes the declaration, the query
    /// planner may use it, and nothing checks that the rows obey it. They are
    /// reported exactly as the catalog states them rather than left out, because
    /// a declared relationship is what a navigator draws its lines from and the
    /// alternative is a client that shows none anywhere.
    ///
    /// `on_update` and `on_delete` are empty because there is nothing to say:
    /// with no enforcement there is no referential action, and writing
    /// `NO ACTION` would be inventing one.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, BigQueryError> {
        let table: Table = self
            .api()
            .get(&rest::table_url(self.project(), schema, relation))
            .await?;
        Ok(table
            .table_constraints
            .map(|constraints| constraints.foreign_keys)
            .unwrap_or_default()
            .into_iter()
            .map(|key| RelationshipInfo {
                name: key.name,
                local_columns: key
                    .column_references
                    .iter()
                    .map(|pair| pair.referencing_column.clone())
                    .collect(),
                other_schema: key.referenced_table.dataset_id,
                other_table: key.referenced_table.table_id,
                other_columns: key
                    .column_references
                    .iter()
                    .map(|pair| pair.referenced_column.clone())
                    .collect(),
                on_update: String::new(),
                on_delete: String::new(),
            })
            .collect())
    }

    /// Empty, always, and without asking.
    ///
    /// The one metadata answer here that is a cost decision rather than a
    /// statement about BigQuery. A table's own resource names what it
    /// references; nothing names what references it. Answering would mean a
    /// `tables.get` for every table in the dataset, or a query against
    /// `INFORMATION_SCHEMA.CONSTRAINT_COLUMN_USAGE` — which is a job, with a
    /// job's latency and a job's bill, every time somebody clicks a table.
    pub async fn referenced_by(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, BigQueryError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. BigQuery has no check, unique or
    /// exclusion constraint; the two constraint-shaped facts it keeps are the
    /// unenforced key declarations, which are already on the columns and in
    /// `foreign_keys`, and `REQUIRED`, which is the column's nullability.
    pub async fn constraints(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<ConstraintInfo>, BigQueryError> {
        Ok(Vec::new())
    }

    /// Empty, always, and without asking. BigQuery has no triggers: there is no
    /// `CREATE TRIGGER` in GoogleSQL and nothing in the catalog to list.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, BigQueryError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, kind: &str) -> TableField {
        TableField {
            name: name.to_string(),
            r#type: kind.to_string(),
            ..TableField::default()
        }
    }

    /// The four words the REST catalog answers with that the SQL beside it does
    /// not accept. A structure pane showing `INTEGER` is describing the API
    /// rather than the database.
    #[test]
    fn a_legacy_type_name_is_shown_as_the_one_a_person_could_type_back() {
        assert_eq!(type_name(&field("n", "INTEGER")), "INT64");
        assert_eq!(type_name(&field("x", "FLOAT")), "FLOAT64");
        assert_eq!(type_name(&field("b", "BOOLEAN")), "BOOL");
        // The ones that are already the same word in both vocabularies.
        assert_eq!(type_name(&field("s", "STRING")), "STRING");
        assert_eq!(type_name(&field("t", "TIMESTAMP")), "TIMESTAMP");
        assert_eq!(type_name(&field("g", "GEOGRAPHY")), "GEOGRAPHY");
        // And one this driver has never met, reported as itself rather than
        // guessed at.
        assert_eq!(type_name(&field("?", "SOMETHING_NEW")), "SOMETHING_NEW");
    }

    /// A parameterised type keeps its parameters, which are separate fields in
    /// this catalog and part of the type everywhere a person writes one.
    #[test]
    fn a_numeric_keeps_the_precision_it_was_declared_with() {
        let mut numeric = field("amount", "NUMERIC");
        numeric.precision = Some("38".to_string());
        numeric.scale = Some("9".to_string());
        assert_eq!(type_name(&numeric), "NUMERIC(38, 9)");

        numeric.scale = None;
        assert_eq!(type_name(&numeric), "NUMERIC(38)");

        // A scale with no precision is not something BigQuery declares, and
        // `NUMERIC(, 9)` is not something anybody can read.
        numeric.precision = None;
        numeric.scale = Some("9".to_string());
        assert_eq!(type_name(&numeric), "NUMERIC");

        let mut text = field("label", "STRING");
        text.max_length = Some("64".to_string());
        assert_eq!(type_name(&text), "STRING(64)");
    }

    /// The two shapes a flat `ColumnInfo` has to describe in one string.
    #[test]
    fn a_repeated_struct_reads_as_the_array_of_structs_it_is() {
        let mut record = field("address", "RECORD");
        record.fields = vec![field("city", "STRING"), field("zip", "INTEGER")];
        assert_eq!(type_name(&record), "STRUCT<city STRING, zip INT64>");

        record.mode = "REPEATED".to_string();
        assert_eq!(type_name(&record), "ARRAY<STRUCT<city STRING, zip INT64>>");

        let mut tags = field("tags", "STRING");
        tags.mode = "REPEATED".to_string();
        assert_eq!(type_name(&tags), "ARRAY<STRING>");
    }

    /// A shape BigQuery has and this workspace's navigator does not is said to
    /// be unrecognised rather than drawn as a table somebody could write to.
    #[test]
    fn a_relation_kind_with_no_icon_is_not_guessed_at() {
        assert_eq!(relation_kind("TABLE"), RelationKind::Table);
        assert_eq!(relation_kind("VIEW"), RelationKind::View);
        assert_eq!(
            relation_kind("MATERIALIZED_VIEW"),
            RelationKind::MaterializedView
        );
        assert_eq!(relation_kind("EXTERNAL"), RelationKind::ForeignTable);
        assert_eq!(relation_kind("SNAPSHOT"), RelationKind::Unknown);
    }
}
