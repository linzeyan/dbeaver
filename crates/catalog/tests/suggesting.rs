//! What gets offered, against a catalog that answers from memory.
//!
//! The driver here is a fake, and that is the point rather than a shortcut: the
//! question under test is what this crate does with an answer, not whether a
//! database gives one. It also counts the calls it is asked to make, which is
//! how the caching claim gets checked — a claim about how often a socket is
//! used cannot be checked by looking at what came back.

use dbcatalog::{Kind, Names};
use dbconn::{
    Browse, ColumnInfo, ConstraintInfo, Cursor, DbResult, Driver, IndexInfo, RelationInfo,
    RelationKind, RelationshipInfo, ResultStream, SchemaInfo, TriggerInfo, TxStep,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A database of two schemas, standing still.
struct Fake {
    calls: AtomicUsize,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl Driver for Fake {
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(["public", "archive"]
            .into_iter()
            .map(|name| SchemaInfo {
                name: name.to_string(),
            })
            .collect())
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let names: &[&str] = match schema {
            "public" => &["orders", "customers", "Order Lines"],
            "archive" => &["old_orders"],
            _ => &[],
        };
        Ok(names
            .iter()
            .map(|name| RelationInfo {
                schema: schema.to_string(),
                name: name.to_string(),
                kind: RelationKind::Table,
                estimated_rows: None,
            })
            .collect())
    }

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let names: &[&str] = match (schema, relation) {
            ("public", "orders") => &["id", "customer_id", "placed_on"],
            ("public", "customers") => &["id", "name"],
            _ => &[],
        };
        Ok(names
            .iter()
            .enumerate()
            .map(|(i, name)| ColumnInfo {
                name: name.to_string(),
                data_type: "integer".to_string(),
                nullable: false,
                position: i as i32 + 1,
                is_primary_key: i == 0,
                default_value: None,
                computed: None,
            })
            .collect())
    }

    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        Ok(None)
    }
    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        Ok(Vec::new())
    }
    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(Vec::new())
    }
    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        Ok(Vec::new())
    }
    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        Ok(Vec::new())
    }
    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        Ok(Vec::new())
    }

    fn browse(&self, _: &Browse<'_>) -> String {
        unreachable!("completion never browses a relation")
    }
    async fn query(&self, _: &str, _: usize) -> DbResult<Box<dyn ResultStream>> {
        unreachable!("completion never runs a statement")
    }
    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("completion never runs a statement")
    }
    async fn cancel(&self) -> DbResult<()> {
        Ok(())
    }
    fn transactional(&self) -> bool {
        false
    }
    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("completion never opens a transaction")
    }
}

/// The labels offered at the `▮`.
async fn offered(marked: &str) -> Vec<String> {
    let (names, _) = catalog();
    labels(&names, marked).await
}

fn catalog() -> (Names, Arc<Fake>) {
    let fake = Fake::new();
    let names = Names::new(fake.clone(), &dbsql::POSTGRES);
    (names, fake)
}

async fn labels(names: &Names, marked: &str) -> Vec<String> {
    let caret = marked.chars().position(|c| c == '▮').expect("no caret") as u32;
    let text = marked.replace('▮', "");
    let question = dbsql::complete(&text, caret, &dbsql::POSTGRES);
    names
        .suggest(&question)
        .await
        .into_iter()
        .map(|s| s.label)
        .collect()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_start_of_a_buffer_offers_verbs_and_not_the_dictionary() {
    let offered = offered("▮").await;
    assert!(offered.contains(&"SELECT".to_string()));
    // Three hundred keywords at an empty caret is a dictionary, not a
    // suggestion list.
    assert!(offered.len() < 30, "{} words offered", offered.len());
}

#[tokio::test]
async fn a_relation_position_offers_the_default_schema_and_the_others_by_name() {
    let offered = offered("SELECT * FROM ▮").await;
    assert!(offered.contains(&"orders".to_string()));
    assert!(offered.contains(&"customers".to_string()));
    // The other schema is offered as a schema, so a table in it is two
    // keystrokes rather than unreachable.
    assert!(offered.contains(&"archive".to_string()));
    // But not its tables, which would put two `orders` in one list with nothing
    // to tell them apart.
    assert!(!offered.contains(&"old_orders".to_string()), "{offered:?}");
}

#[tokio::test]
async fn a_qualified_relation_position_offers_only_that_schema() {
    let offered = offered("SELECT * FROM archive.▮").await;
    assert_eq!(offered, ["old_orders"]);
}

#[tokio::test]
async fn a_column_position_offers_the_columns_of_what_is_in_scope() {
    let offered = offered("SELECT ▮ FROM orders o").await;
    assert_eq!(offered, ["customer_id", "id", "placed_on"]);
}

#[tokio::test]
async fn a_qualifier_narrows_to_one_relation() {
    let both = offered("SELECT ▮ FROM orders o JOIN customers c ON c.id = o.customer_id").await;
    assert!(both.contains(&"placed_on".to_string()));
    assert!(both.contains(&"name".to_string()));

    let one = offered("SELECT c.▮ FROM orders o JOIN customers c ON c.id = o.customer_id").await;
    assert_eq!(one, ["id", "name"]);
}

#[tokio::test]
async fn a_qualifier_that_names_nothing_offers_nothing() {
    // Offering every column in the database after `x.` where `x` is a typo is
    // worse than an empty list, which at least says the qualifier is wrong.
    let offered = offered("SELECT x.▮ FROM orders o").await;
    assert!(offered.is_empty(), "{offered:?}");
}

#[tokio::test]
async fn what_has_been_typed_narrows_and_orders_the_list() {
    let narrowed = offered("SELECT cust▮ FROM orders o").await;
    assert_eq!(narrowed, ["customer_id"]);

    // A name that merely contains the text is worth offering — somebody looking
    // for `customer_id` may type `id` — and worth keeping below the ones that
    // start with it.
    let ordered = offered("SELECT id▮ FROM orders o").await;
    assert_eq!(ordered, ["id", "customer_id"]);
}

#[tokio::test]
async fn there_is_nothing_to_offer_inside_a_literal() {
    assert!(offered("SELECT 'a▮' FROM orders").await.is_empty());
}

#[tokio::test]
async fn a_name_the_statement_invented_is_offered_as_its_own_thing() {
    let (names, _) = catalog();
    let caret = "WITH recent AS (SELECT 1) SELECT ".chars().count() as u32;
    let text = "WITH recent AS (SELECT 1) SELECT  FROM recent";
    let question = dbsql::complete(text, caret, &dbsql::POSTGRES);
    let offered = names.suggest(&question).await;
    let cte = offered
        .iter()
        .find(|s| s.label == "recent")
        .expect("the CTE should be offered");
    // Not a relation: the catalog has never heard of it, and a front end that
    // showed a table icon would be claiming it could be browsed.
    assert_eq!(cte.kind, Kind::Local);
}

// ---------------------------------------------------------------------------
// The caching claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_same_question_asked_again_does_not_reach_the_database() {
    // The claim the design rests on. Completion runs on a keystroke, and a
    // metadata call per keystroke is a client that pauses while you type.
    let (names, fake) = catalog();
    labels(&names, "SELECT ▮ FROM orders o").await;
    let after_first = fake.calls.load(Ordering::SeqCst);
    assert!(after_first > 0, "the first question has to ask");

    for _ in 0..50 {
        labels(&names, "SELECT ▮ FROM orders o").await;
    }
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        after_first,
        "a remembered answer was fetched again"
    );
}

#[tokio::test]
async fn typing_one_more_character_asks_nothing_new() {
    let (names, fake) = catalog();
    labels(&names, "SELECT ▮ FROM orders o").await;
    let settled = fake.calls.load(Ordering::SeqCst);
    for typed in ["c▮", "cu▮", "cus▮", "cust▮", "custo▮"] {
        labels(&names, &format!("SELECT {typed} FROM orders o")).await;
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), settled);
}

#[tokio::test]
async fn a_refresh_the_user_asked_for_makes_the_next_question_ask_again() {
    let (names, fake) = catalog();
    labels(&names, "SELECT ▮ FROM orders o").await;
    let settled = fake.calls.load(Ordering::SeqCst);
    names.forget().await;
    labels(&names, "SELECT ▮ FROM orders o").await;
    assert!(fake.calls.load(Ordering::SeqCst) > settled);
}

// ---------------------------------------------------------------------------
// Inserting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_name_that_needs_quoting_is_offered_quoted_and_shown_plain() {
    let (names, _) = catalog();
    let text = "SELECT * FROM Order";
    let question = dbsql::complete(text, text.chars().count() as u32, &dbsql::POSTGRES);
    let found = names
        .suggest(&question)
        .await
        .into_iter()
        .find(|s| s.label == "Order Lines")
        .expect("the table should be offered");
    // Shown as the catalog holds it, inserted as this database will read it.
    assert_eq!(found.insert, "\"Order Lines\"");
}

#[tokio::test]
async fn an_ordinary_name_is_not_quoted_for_the_sake_of_it() {
    let (names, _) = catalog();
    let question = dbsql::complete("SELECT * FROM ", 14, &dbsql::POSTGRES);
    let found = names
        .suggest(&question)
        .await
        .into_iter()
        .find(|s| s.label == "orders")
        .expect("the table should be offered");
    assert_eq!(found.insert, "orders");
}
