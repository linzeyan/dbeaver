//! What `read` asks a connection, against a driver that answers from memory and
//! counts what it was asked.
//!
//! The unit tests in `src/lib.rs` cover `compare`, which needs no server. This
//! half needs one, and a fake is the point rather than a shortcut: two of the
//! claims here are about *how many* questions get asked and one is about what
//! happens when a question is refused, and neither can be checked by looking at
//! what came back from a real database.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dbconn::{
    Browse, Capabilities, ColumnInfo, ConstraintInfo, ConstraintKind, Cursor, DatabaseInfo,
    DbError, DbResult, Driver, IndexInfo, RelationInfo, RelationKind, RelationshipInfo,
    ResultStream, SchemaInfo, ServerInfo, ServerProcesses, TriggerInfo, TxStep, UniqueKeyInfo,
};

/// A schema of one table and one view, standing still.
struct Fake {
    /// How many times each of the three per-relation reads was asked for, so a
    /// claim about round trips has something to be false against.
    indexes: AtomicUsize,
    constraints: AtomicUsize,
    foreign_keys: AtomicUsize,
    /// Which call, if any, refuses. `read` must pass a refusal on rather than
    /// turn it into an empty list.
    refuses: Option<&'static str>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            indexes: AtomicUsize::new(0),
            constraints: AtomicUsize::new(0),
            foreign_keys: AtomicUsize::new(0),
            refuses: None,
        })
    }

    fn refusing(call: &'static str) -> Arc<Self> {
        Arc::new(Self {
            indexes: AtomicUsize::new(0),
            constraints: AtomicUsize::new(0),
            foreign_keys: AtomicUsize::new(0),
            refuses: Some(call),
        })
    }

    fn refusal(&self, call: &str) -> DbResult<()> {
        if self.refuses == Some(call) {
            return Err(DbError::new(format!("no rights to read {call}")));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Driver for Fake {
    async fn relations(&self, _: &str) -> DbResult<Vec<RelationInfo>> {
        self.refusal("relations")?;
        // Deliberately out of name order, and with the view first. Both halves
        // matter: the report is read top to bottom, and two servers that list
        // the same relations in different orders must not produce two
        // different-looking reports of no differences.
        Ok(vec![
            RelationInfo {
                schema: "public".into(),
                name: "paid".into(),
                kind: RelationKind::View,
                estimated_rows: None,
            },
            RelationInfo {
                schema: "public".into(),
                name: "invoice".into(),
                kind: RelationKind::Table,
                estimated_rows: None,
            },
        ])
    }

    async fn columns(&self, _: &str, _: &str) -> DbResult<Vec<ColumnInfo>> {
        self.refusal("columns")?;
        Ok(vec![ColumnInfo {
            name: "id".into(),
            data_type: "integer".into(),
            nullable: false,
            position: 1,
            is_primary_key: true,
            default_value: None,
            computed: None,
        }])
    }

    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        self.indexes.fetch_add(1, Ordering::SeqCst);
        self.refusal("indexes")?;
        Ok(Vec::new())
    }

    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        self.constraints.fetch_add(1, Ordering::SeqCst);
        self.refusal("constraints")?;
        Ok(vec![ConstraintInfo {
            name: "invoice_pkey".into(),
            kind: ConstraintKind::Unique,
            definition: "UNIQUE (id)".into(),
        }])
    }

    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        self.foreign_keys.fetch_add(1, Ordering::SeqCst);
        self.refusal("foreign_keys")?;
        Ok(Vec::new())
    }

    async fn server_info(&self) -> DbResult<ServerInfo> {
        unreachable!("a comparison never asks the server who it is")
    }
    async fn databases(&self) -> DbResult<Option<Vec<DatabaseInfo>>> {
        Ok(None)
    }
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        unreachable!("a comparison is handed the two schema names")
    }
    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        unreachable!("a view's defining statement is deliberately not compared")
    }
    async fn unique_keys(&self, _: &str, _: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        unreachable!("the key is read off the columns")
    }
    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("a foreign key belongs to the table that declares it, once")
    }
    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        unreachable!("triggers are not compared")
    }
    fn browse(&self, _: &Browse<'_>) -> String {
        unreachable!("a comparison never reads a row")
    }
    async fn query(&self, _: &str, _: usize) -> DbResult<Box<dyn ResultStream>> {
        unreachable!("a comparison never runs a statement")
    }
    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("a comparison never runs a statement")
    }
    async fn cancel(&self) -> DbResult<()> {
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional: false,
            cancel_stops_the_statement: false,
            switches_database: false,
            schema_is_the_database: false,
            reports_routines: false,
            reports_sequences: false,
            server_processes: ServerProcesses::Unreported,
            reports_variables: false,
            writes_rows: false,
        }
    }
    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("a comparison never opens a transaction")
    }
}

/// A view is asked for its columns and nothing else.
///
/// Three round trips per view to be told it has no index, no constraint and no
/// foreign key is the whole cost of asking, and none of it changes a report:
/// this is the claim that cannot be seen in the answer, only in the count.
#[tokio::test]
async fn a_view_is_not_asked_what_it_has_no_way_of_having() {
    let fake = Fake::new();
    let side = dbdiff::read(fake.as_ref(), "public")
        .await
        .expect("the fake answers");
    assert_eq!(side.tables.len(), 2);
    assert_eq!(
        fake.indexes.load(Ordering::SeqCst),
        1,
        "the table, not the view"
    );
    assert_eq!(fake.constraints.load(Ordering::SeqCst), 1);
    assert_eq!(fake.foreign_keys.load(Ordering::SeqCst), 1);

    // And the one that was asked is the one that could have answered.
    let table = side
        .tables
        .iter()
        .find(|t| t.name == "invoice")
        .expect("the table is there");
    assert_eq!(table.constraints.len(), 1);
    let view = side
        .tables
        .iter()
        .find(|t| t.name == "paid")
        .expect("the view is there");
    assert!(view.constraints.is_empty());
    assert_eq!(view.columns.len(), 1, "a view still has columns to compare");
}

/// Sorted here rather than trusted from the driver, so that two servers listing
/// the same relations in different orders do not produce two different-looking
/// reports of no differences.
#[tokio::test]
async fn the_relations_come_back_in_name_order_whatever_order_they_arrived_in() {
    let fake = Fake::new();
    let side = dbdiff::read(fake.as_ref(), "public")
        .await
        .expect("the fake answers");
    assert_eq!(
        side.tables
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["invoice", "paid"]
    );
}

/// A refusal is passed on rather than becoming an empty side.
///
/// This is the failure that would be worst as a success: a login without rights
/// to one of the two schemas would otherwise produce a report saying every
/// relation in it had been dropped — the same shape as real news, and not news
/// at all.
#[tokio::test]
async fn a_question_the_server_refuses_fails_the_read_rather_than_emptying_it() {
    for call in [
        "relations",
        "columns",
        "indexes",
        "constraints",
        "foreign_keys",
    ] {
        let fake = Fake::refusing(call);
        let refused = dbdiff::read(fake.as_ref(), "public").await;
        assert!(refused.is_err(), "{call} was refused and the read went on");
    }
}
