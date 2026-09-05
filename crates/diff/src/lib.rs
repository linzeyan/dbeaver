//! What two schemas do not agree about.
//!
//! Two halves, and the split is what makes this testable. [`read`] asks a
//! connection what is in a schema, which needs a server; [`compare`] takes two of
//! those answers and says where they differ, which needs nothing — so the rules
//! that decide what counts as a difference are pinned by unit tests here rather
//! than by whatever two databases happen to be running.
//!
//! Every difference is stated as the two sides' own descriptions of one object.
//! That is one decision doing two jobs: a change is *detected* by the
//! descriptions differing and *shown* as those descriptions, so there is no list
//! of compared fields that can drift from the list of displayed ones. A field
//! this build does not write into a description is a field it does not claim to
//! compare, and both facts move together.
//!
//! What it does not do is write the SQL to reconcile the two. That is a second
//! job with its own hazards — ordering, dependencies, the columns whose change
//! cannot be made without losing what is in them — and a report that is honest
//! about a difference is worth more than a script that is confident about it.

use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DbResult, Driver, IndexInfo, RelationInfo,
    RelationKind, RelationshipInfo,
};

/// One relation and everything about it this compares.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub kind: RelationKind,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub constraints: Vec<ConstraintInfo>,
    pub foreign_keys: Vec<RelationshipInfo>,
}

/// One schema, as one connection reported it.
#[derive(Debug, Clone, Default)]
pub struct Side {
    pub tables: Vec<Table>,
}

/// What kind of object a difference is about, so a reader can tell a missing
/// table from a missing column at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Relation,
    Column,
    Index,
    Constraint,
    ForeignKey,
}

/// Which side has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    OnlyLeft,
    OnlyRight,
    Changed,
}

/// One thing the two schemas do not agree about.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Difference {
    /// The relation this is about, so the report can be read table by table.
    pub table: String,
    /// What the difference names, within that relation: a column's name, an
    /// index's, the relation's own where the whole thing is missing.
    pub object: String,
    pub kind: Kind,
    pub verdict: Verdict,
    /// How each side describes it, and empty on the side that does not have it.
    pub left: String,
    pub right: String,
}

/// Two schemas compared, with enough context that an empty list means something.
///
/// The counts are here because "no differences" and "nothing was read" look
/// identical in a list and are not: a login that can see no relations in the
/// schema it named produces the first shape and deserves the second sentence.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Report {
    pub left_relations: usize,
    pub right_relations: usize,
    pub differences: Vec<Difference>,
}

/// What is in `schema` on this connection.
///
/// Every failure is passed on rather than turned into an empty list. A schema
/// whose indexes could not be read would otherwise be reported as a schema whose
/// indexes had all been dropped, which is the same shape as real news and is not
/// news at all.
pub async fn read(driver: &dyn Driver, schema: &str) -> DbResult<Side> {
    let mut tables = Vec::new();
    for relation in driver.relations(schema).await? {
        tables.push(table(driver, schema, relation).await?);
    }
    // Sorted here rather than trusted from the driver: the report is read
    // top to bottom, and two servers that list the same relations in different
    // orders must not produce two different-looking reports of no differences.
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Side { tables })
}

async fn table(driver: &dyn Driver, schema: &str, relation: RelationInfo) -> DbResult<Table> {
    let name = relation.name;
    let columns = driver.columns(schema, &name).await?;
    // A view has no index, constraint or foreign key of its own, and asking is
    // three round trips per view to be told so. The columns are the whole of
    // what a view has to compare — its defining statement is deliberately not
    // here, because the two databases render it back differently enough that
    // comparing the text would report every view as changed.
    let structural = matches!(
        relation.kind,
        RelationKind::Table | RelationKind::PartitionedTable | RelationKind::ForeignTable
    );
    Ok(Table {
        kind: relation.kind,
        indexes: if structural {
            driver.indexes(schema, &name).await?
        } else {
            Vec::new()
        },
        constraints: if structural {
            driver.constraints(schema, &name).await?
        } else {
            Vec::new()
        },
        foreign_keys: if structural {
            driver.foreign_keys(schema, &name).await?
        } else {
            Vec::new()
        },
        columns,
        name,
    })
}

/// Where `left` and `right` disagree, in the order a report is read.
///
/// Objects are matched by name, which is what a migration would have to say out
/// loud. Two indexes over the same columns under different names come back as one
/// removed and one added, because that is what they are to anything that would
/// have to write the change.
pub fn compare(left: &Side, right: &Side) -> Report {
    let mut differences = Vec::new();
    for name in names(
        left.tables.iter().map(|t| &t.name),
        right.tables.iter().map(|t| &t.name),
    ) {
        let this = left.tables.iter().find(|t| t.name == name);
        let that = right.tables.iter().find(|t| t.name == name);
        match (this, that) {
            // A relation on one side only is one line, not one line per column
            // it has. Forty rows saying "this column is missing too" is the same
            // news written forty times, and it buries the thirty-ninth table.
            (Some(only), None) => differences.push(Difference {
                table: name.clone(),
                object: name,
                kind: Kind::Relation,
                verdict: Verdict::OnlyLeft,
                left: kind_word(only.kind).to_string(),
                right: String::new(),
            }),
            (None, Some(only)) => differences.push(Difference {
                table: name.clone(),
                object: name,
                kind: Kind::Relation,
                verdict: Verdict::OnlyRight,
                left: String::new(),
                right: kind_word(only.kind).to_string(),
            }),
            (Some(this), Some(that)) => compare_tables(this, that, &mut differences),
            (None, None) => unreachable!("a name came from one of the two lists"),
        }
    }
    Report {
        left_relations: left.tables.len(),
        right_relations: right.tables.len(),
        differences,
    }
}

fn compare_tables(left: &Table, right: &Table, into: &mut Vec<Difference>) {
    if left.kind != right.kind {
        into.push(Difference {
            table: left.name.clone(),
            object: left.name.clone(),
            kind: Kind::Relation,
            verdict: Verdict::Changed,
            left: kind_word(left.kind).to_string(),
            right: kind_word(right.kind).to_string(),
        });
    }
    // Columns first, then the things declared over them, because that is the
    // order somebody reads a table in and the order a change would have to be
    // made in.
    members(
        &left.name,
        Kind::Column,
        left.columns.iter().map(|c| (c.name.clone(), column(c))),
        right.columns.iter().map(|c| (c.name.clone(), column(c))),
        into,
    );
    members(
        &left.name,
        Kind::Index,
        left.indexes.iter().map(|i| (i.name.clone(), index(i))),
        right.indexes.iter().map(|i| (i.name.clone(), index(i))),
        into,
    );
    members(
        &left.name,
        Kind::Constraint,
        left.constraints
            .iter()
            .map(|c| (c.name.clone(), constraint(c))),
        right
            .constraints
            .iter()
            .map(|c| (c.name.clone(), constraint(c))),
        into,
    );
    members(
        &left.name,
        Kind::ForeignKey,
        left.foreign_keys
            .iter()
            .map(|k| (k.name.clone(), foreign_key(k))),
        right
            .foreign_keys
            .iter()
            .map(|k| (k.name.clone(), foreign_key(k))),
        into,
    );
}

/// One list of named descriptions against another.
///
/// The whole comparison is here: an object is the same object as the one with
/// its name, and it has changed when the two sides describe it differently.
fn members(
    table: &str,
    kind: Kind,
    left: impl Iterator<Item = (String, String)>,
    right: impl Iterator<Item = (String, String)>,
    into: &mut Vec<Difference>,
) {
    let left: Vec<_> = left.collect();
    let right: Vec<_> = right.collect();
    for name in names(left.iter().map(|(n, _)| n), right.iter().map(|(n, _)| n)) {
        let this = left.iter().find(|(n, _)| *n == name).map(|(_, d)| d);
        let that = right.iter().find(|(n, _)| *n == name).map(|(_, d)| d);
        let verdict = match (this, that) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(_), Some(_)) => Verdict::Changed,
            (Some(_), None) => Verdict::OnlyLeft,
            (None, Some(_)) => Verdict::OnlyRight,
            (None, None) => continue,
        };
        into.push(Difference {
            table: table.to_string(),
            object: name,
            kind,
            verdict,
            left: this.cloned().unwrap_or_default(),
            right: that.cloned().unwrap_or_default(),
        });
    }
}

/// Every name in either list, in order, once each.
///
/// Sorted rather than left in catalog order, so that the report of two schemas
/// reads the same whichever of them was asked first.
fn names<'a>(
    left: impl Iterator<Item = &'a String>,
    right: impl Iterator<Item = &'a String>,
) -> Vec<String> {
    let mut all: Vec<String> = left.chain(right).cloned().collect();
    all.sort();
    all.dedup();
    all
}

/// A column as the side holding it describes it.
///
/// Deliberately without the position. A column added in the middle of a table
/// shifts every column after it on the databases that renumber, and a comparison
/// that read position would report thirty columns as changed to say one was
/// inserted — burying the one line that is news under twenty-nine that are not.
///
/// The primary-key mark is in, because a column that stopped being part of the
/// key is a difference somebody has to know about, and it is the only place this
/// build reads the key from.
fn column(info: &ColumnInfo) -> String {
    let mut said = info.data_type.clone();
    if !info.nullable {
        said.push_str(" not null");
    }
    if info.is_primary_key {
        said.push_str(" primary key");
    }
    match (&info.default_value, info.computed) {
        (Some(expression), Some(_)) => {
            said.push_str(" computed ");
            said.push_str(expression);
        }
        (Some(value), None) => {
            said.push_str(" default ");
            said.push_str(value);
        }
        (None, _) => {}
    }
    said
}

fn index(info: &IndexInfo) -> String {
    let mut said = String::new();
    if info.is_unique {
        said.push_str("unique ");
    }
    said.push_str(&info.method);
    said.push_str(" (");
    said.push_str(&info.columns.join(", "));
    said.push(')');
    if let Some(predicate) = &info.predicate {
        said.push_str(" where ");
        said.push_str(predicate);
    }
    said
}

/// A constraint as the side holding it describes it.
///
/// The kind is prefixed only where the definition does not already open with it.
/// PostgreSQL hands back `pg_get_constraintdef`, which starts with the word —
/// `CHECK ((qty > 0))` — and "check CHECK ((qty > 0))" is a stutter that reads as
/// a bug in the report. Servers that hand back only the expression are why the
/// word is added at all, and this is the same test either way: the description
/// says what kind of constraint it is, exactly once.
fn constraint(info: &ConstraintInfo) -> String {
    let word = kind_of(info.kind);
    let said_already = info
        .definition
        .split_whitespace()
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(word));
    if said_already {
        return info.definition.clone();
    }
    format!("{word} {}", info.definition)
}

fn kind_of(kind: ConstraintKind) -> &'static str {
    match kind {
        ConstraintKind::Check => "check",
        ConstraintKind::Unique => "unique",
        ConstraintKind::Exclude => "exclude",
        ConstraintKind::Other => "constraint",
    }
}

fn foreign_key(info: &RelationshipInfo) -> String {
    format!(
        "({}) -> {}.{} ({}) on update {} on delete {}",
        info.local_columns.join(", "),
        info.other_schema,
        info.other_table,
        info.other_columns.join(", "),
        info.on_update,
        info.on_delete
    )
}

/// What a relation is, in one word a report can print.
///
/// Its own list rather than the serde name, because the two are read by different
/// audiences: serde's is a wire format the front end matches on, and a rename
/// there should not silently become a change of what somebody reads.
fn kind_word(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "materialized view",
        RelationKind::ForeignTable => "foreign table",
        RelationKind::PartitionedTable => "partitioned table",
        RelationKind::Virtual => "virtual table",
        RelationKind::Unknown => "relation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column_info(name: &str, data_type: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
            position: 1,
            is_primary_key: false,
            default_value: None,
            computed: None,
        }
    }

    fn table(name: &str, columns: Vec<ColumnInfo>) -> Table {
        Table {
            name: name.to_string(),
            kind: RelationKind::Table,
            columns,
            indexes: Vec::new(),
            constraints: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    fn side(tables: Vec<Table>) -> Side {
        Side { tables }
    }

    #[test]
    fn two_schemas_that_agree_have_nothing_to_report() {
        let one = || side(vec![table("orders", vec![column_info("id", "integer")])]);
        let report = compare(&one(), &one());
        assert_eq!(report.differences, vec![]);
        // And the counts still say what was looked at, which is what makes the
        // empty list readable as an answer rather than as a failure.
        assert_eq!((report.left_relations, report.right_relations), (1, 1));
    }

    /// A table on one side is one line. The alternative — a line per column it
    /// has — writes the same news forty times and buries the next table.
    #[test]
    fn a_relation_on_one_side_is_reported_once_and_not_column_by_column() {
        let report = compare(
            &side(vec![table(
                "orders",
                vec![
                    column_info("id", "integer"),
                    column_info("total", "numeric"),
                ],
            )]),
            &side(vec![]),
        );
        assert_eq!(report.differences.len(), 1);
        let only = &report.differences[0];
        assert_eq!(only.kind, Kind::Relation);
        assert_eq!(only.verdict, Verdict::OnlyLeft);
        assert_eq!(only.object, "orders");
        assert_eq!((only.left.as_str(), only.right.as_str()), ("table", ""));
    }

    #[test]
    fn a_column_that_changed_says_what_each_side_calls_it() {
        let mut changed = column_info("total", "numeric(18,4)");
        changed.nullable = false;
        let report = compare(
            &side(vec![table(
                "orders",
                vec![column_info("total", "numeric(18,4)")],
            )]),
            &side(vec![table("orders", vec![changed])]),
        );
        assert_eq!(report.differences.len(), 1);
        let one = &report.differences[0];
        assert_eq!(one.table, "orders");
        assert_eq!(one.object, "total");
        assert_eq!(one.kind, Kind::Column);
        assert_eq!(one.verdict, Verdict::Changed);
        assert_eq!(one.left, "numeric(18,4)");
        assert_eq!(one.right, "numeric(18,4) not null");
    }

    /// The position is the field this deliberately does not read: inserting one
    /// column would otherwise report every column after it as changed.
    #[test]
    fn a_column_that_only_moved_is_not_a_difference() {
        let mut moved = column_info("total", "numeric");
        moved.position = 7;
        let report = compare(
            &side(vec![table("orders", vec![column_info("total", "numeric")])]),
            &side(vec![table("orders", vec![moved])]),
        );
        assert_eq!(report.differences, vec![]);
    }

    /// A default and a computation are both held in `default_value`, and only
    /// one of them can be written back as a default. Describing them the same
    /// way would make a column that became computed read as unchanged.
    #[test]
    fn a_default_and_a_computation_are_not_the_same_description() {
        let mut defaulted = column_info("total", "integer");
        defaulted.default_value = Some("(a + b)".into());
        let mut computed = defaulted.clone();
        computed.computed = Some(dbconn::Computed::Stored);
        let report = compare(
            &side(vec![table("orders", vec![defaulted])]),
            &side(vec![table("orders", vec![computed])]),
        );
        assert_eq!(report.differences.len(), 1);
        assert_eq!(report.differences[0].left, "integer default (a + b)");
        assert_eq!(report.differences[0].right, "integer computed (a + b)");
    }

    /// Renaming an index is two differences and not none: whatever would have to
    /// make the two schemas match has to drop one name and create the other.
    #[test]
    fn an_index_under_another_name_is_one_removed_and_one_added() {
        let make = |name: &str| IndexInfo {
            name: name.to_string(),
            columns: vec!["email".into()],
            is_unique: true,
            is_primary: false,
            method: "btree".into(),
            predicate: None,
        };
        let mut left = table("orders", vec![]);
        left.indexes = vec![make("orders_email_idx")];
        let mut right = table("orders", vec![]);
        right.indexes = vec![make("ix_orders_email")];
        let report = compare(&side(vec![left]), &side(vec![right]));
        assert_eq!(report.differences.len(), 2);
        assert_eq!(report.differences[0].object, "ix_orders_email");
        assert_eq!(report.differences[0].verdict, Verdict::OnlyRight);
        assert_eq!(report.differences[1].object, "orders_email_idx");
        assert_eq!(report.differences[1].verdict, Verdict::OnlyLeft);
        // Both descriptions are the index as its own side states it, so a reader
        // can see at a glance that the two are the same index twice named.
        assert_eq!(report.differences[0].right, "unique btree (email)");
        assert_eq!(report.differences[1].left, "unique btree (email)");
    }

    /// A partial index over the same columns is not the same index, and a
    /// description that stopped at the columns would call it one.
    #[test]
    fn an_index_narrowed_by_a_predicate_has_changed() {
        let make = |predicate: Option<&str>| IndexInfo {
            name: "orders_open_idx".into(),
            columns: vec!["id".into()],
            is_unique: false,
            is_primary: false,
            method: "btree".into(),
            predicate: predicate.map(str::to_string),
        };
        let mut left = table("orders", vec![]);
        left.indexes = vec![make(None)];
        let mut right = table("orders", vec![]);
        right.indexes = vec![make(Some("shipped_at IS NULL"))];
        let report = compare(&side(vec![left]), &side(vec![right]));
        assert_eq!(report.differences.len(), 1);
        assert_eq!(report.differences[0].verdict, Verdict::Changed);
        assert_eq!(
            report.differences[0].right,
            "btree (id) where shipped_at IS NULL"
        );
    }

    #[test]
    fn a_foreign_key_states_both_ends_and_both_actions() {
        let make = |on_delete: &str| RelationshipInfo {
            name: "orders_customer_fkey".into(),
            local_columns: vec!["customer_id".into()],
            other_schema: "public".into(),
            other_table: "customers".into(),
            other_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: on_delete.to_string(),
        };
        let mut left = table("orders", vec![]);
        left.foreign_keys = vec![make("NO ACTION")];
        let mut right = table("orders", vec![]);
        right.foreign_keys = vec![make("CASCADE")];
        let report = compare(&side(vec![left]), &side(vec![right]));
        assert_eq!(report.differences.len(), 1);
        assert_eq!(report.differences[0].kind, Kind::ForeignKey);
        assert_eq!(
            report.differences[0].right,
            "(customer_id) -> public.customers (id) on update NO ACTION on delete CASCADE"
        );
    }

    #[test]
    fn a_table_that_became_a_view_is_reported_as_the_change_it_is() {
        let mut right = table("orders", vec![column_info("id", "integer")]);
        right.kind = RelationKind::View;
        let report = compare(
            &side(vec![table("orders", vec![column_info("id", "integer")])]),
            &side(vec![right]),
        );
        assert_eq!(report.differences.len(), 1);
        assert_eq!(report.differences[0].kind, Kind::Relation);
        assert_eq!(report.differences[0].verdict, Verdict::Changed);
        assert_eq!(
            (
                report.differences[0].left.as_str(),
                report.differences[0].right.as_str()
            ),
            ("table", "view")
        );
    }

    /// The report is read top to bottom, so it has to be the same report
    /// whichever order the catalogs happened to answer in.
    #[test]
    fn the_report_reads_the_same_whichever_order_the_catalog_answered_in() {
        let columns = || vec![column_info("b", "integer"), column_info("a", "text")];
        let forwards = side(vec![table("zebra", columns()), table("apple", vec![])]);
        let backwards = side(vec![table("apple", vec![]), table("zebra", vec![])]);
        let report = compare(&forwards, &backwards);
        assert_eq!(
            report
                .differences
                .iter()
                .map(|d| (d.table.as_str(), d.object.as_str()))
                .collect::<Vec<_>>(),
            vec![("zebra", "a"), ("zebra", "b")]
        );
    }

    /// The kind is said once, whichever of the two shapes a server hands back.
    #[test]
    fn a_constraint_names_what_kind_it_is_exactly_once() {
        let described = |definition: &str| {
            let mut left = table("orders", vec![]);
            left.constraints = vec![ConstraintInfo {
                name: "orders_qty_positive".into(),
                kind: ConstraintKind::Check,
                definition: definition.into(),
            }];
            let report = compare(&side(vec![left]), &side(vec![table("orders", vec![])]));
            assert_eq!(report.differences.len(), 1);
            assert_eq!(report.differences[0].kind, Kind::Constraint);
            report.differences[0].left.clone()
        };
        // PostgreSQL's `pg_get_constraintdef` opens with the word itself, and
        // "check CHECK ((qty > 0))" reads as a bug in the report rather than as
        // a constraint.
        assert_eq!(described("CHECK ((qty > 0))"), "CHECK ((qty > 0))");
        // A server that hands back only the expression is why the word is added.
        assert_eq!(described("(qty > 0)"), "check (qty > 0)");
        // And a column that happens to start with the letters is not the word.
        assert_eq!(described("checked > 0"), "check checked > 0");
    }
}
