//! What a server says when it is asked how it would run a statement.
//!
//! Two questions, and they are not the same one. [`dialect::Dialect::explain_prefix`]
//! says how a *dialect* asks for a plan in words — `EXPLAIN`, which every product
//! speaking that dialect takes, and which comes back as prose for somebody to
//! read. This module is about the machine-readable form, which is a fact about
//! the *product* rather than the dialect: CockroachDB and GreptimeDB arrive
//! through the PostgreSQL driver and reject `EXPLAIN (FORMAT JSON)` outright, and
//! StarRocks arrives through MySQL's and rejects `EXPLAIN FORMAT=JSON`. A table
//! keyed by scheme would answer for all of them at once and be wrong for half.
//!
//! So the key is the product name the connection reported, and the table only
//! holds products this build has run the statement against. Everything else gets
//! `None`, which means the front end offers the prose plan it always did — not a
//! guess that fails at the server.
//!
//! What comes back is normalised into [`Plan`], because the two shapes here have
//! nothing in common on the wire: PostgreSQL answers with one cell holding a JSON
//! document, SQLite with four columns of rows that name their own parents. The
//! part that is the same in both is the only part a reader wants — a tree of
//! steps, each with what it does and what it is expected to cost.

use serde_json::{Map, Value};

/// One step of a plan, and the steps that feed it.
///
/// The server's own words throughout. Nothing here is rewritten into a house
/// vocabulary: a reader comparing this against `psql`'s output or against the
/// database's documentation has to find the same nouns, and a client that renamed
/// `Merge Join` to something tidier would be the only place in the world using
/// that name.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Plan {
    /// What this step does: `Merge Join`, `SCAN c USING COVERING INDEX c_parent`.
    pub label: String,
    /// The rest of what the server said about this step, one fact per line,
    /// already written as `Key: value`.
    ///
    /// Not a chosen list of interesting keys. A whitelist goes stale the release
    /// after it is written — every PostgreSQL version adds node properties — and
    /// the ones it would drop are exactly the ones somebody is squinting at when
    /// a plan surprises them.
    pub detail: Vec<String>,
    /// Rows the planner expects out of this step, where it said.
    pub rows: Option<f64>,
    /// What the planner expects the step to cost, in whatever units it counts in,
    /// including everything below it.
    pub cost: Option<f64>,
    /// `cost` minus the cost of the steps feeding this one.
    ///
    /// Derived here rather than in a front end because the thing being corrected
    /// for is a fact about the server: PostgreSQL's `Total Cost` is cumulative, so
    /// the root always holds the largest number in the tree and a bar drawn from
    /// it says only that the root is the root. What a reader is looking for is the
    /// step that added the cost, which is this.
    ///
    /// Floored at zero rather than allowed to go negative. A nested loop's inner
    /// side is costed per execution and charged to the join by the number of
    /// loops, so the subtraction can overshoot — and a negative cost is not a
    /// smaller number, it is a number that means nothing.
    pub self_cost: Option<f64>,
    pub children: Vec<Plan>,
}

/// How a product answers, for the two that answer at all.
///
/// Private: a caller asks about a product and gets a prefix or a tree, and the
/// shape is this module's business. Making it public would put a second name for
/// each product into the front end, which would then have two ways to be wrong
/// about one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One row, one cell, holding the document `EXPLAIN (FORMAT JSON)` returns.
    PostgresJson,
    /// Four columns — id, parent, notused, detail — one row per step, each naming
    /// the step it belongs under. `EXPLAIN QUERY PLAN`.
    SqliteRows,
}

impl Shape {
    /// What is written in front of the statement to ask for this shape.
    ///
    /// Paired with the shape rather than kept in a table beside it, so that a
    /// caller cannot ask one way and read the answer the other.
    fn prefix(self) -> &'static str {
        match self {
            Shape::PostgresJson => "EXPLAIN (FORMAT JSON)",
            Shape::SqliteRows => "EXPLAIN QUERY PLAN",
        }
    }
}

/// What `product` answers with, or `None` where this build has not established
/// that it answers at all.
///
/// Matched on the name a driver reports from the server's own banner, which is
/// how a `postgres://` connection to CockroachDB is told from one to PostgreSQL.
///
/// MySQL is deliberately absent. It has the form — `EXPLAIN FORMAT=JSON` — and
/// the driver cannot tell MySQL from StarRocks, which does not: StarRocks answers
/// `SELECT VERSION()` with a MySQL version and nothing of its own, so the driver
/// reports both as "MySQL" rather than printing a guess as a fact. Answering yes
/// for that name would hand every StarRocks user a syntax error in place of the
/// plan they have today. It belongs here the day something can tell the two
/// apart.
fn shape(product: &str) -> Option<Shape> {
    match product {
        "PostgreSQL" => Some(Shape::PostgresJson),
        "SQLite" => Some(Shape::SqliteRows),
        _ => None,
    }
}

/// What is written in front of a statement to ask `product` for a plan it can be
/// drawn from, or `None` where there is no such request to make.
///
/// `None` is an answer: the caller falls back to the dialect's prose `EXPLAIN`,
/// which is what it sent before this module existed.
pub fn prefix(product: &str) -> Option<&'static str> {
    shape(product).map(Shape::prefix)
}

/// The rows [`prefix`] asked for, read as a tree.
///
/// A forest rather than a single root, because SQLite's is one: a statement with
/// a `GROUP BY` and an `ORDER BY` produces several top-level steps, and wrapping
/// them in an invented parent would put a step on screen that no server said.
/// PostgreSQL's answer is one tree, which is a forest of one.
///
/// `None` where there is nothing to draw — an unknown product, an empty result, a
/// document that is not the shape this product was supposed to answer in. The
/// caller shows the rows themselves, which are still on screen and still true.
pub fn read(product: &str, rows: &[Vec<String>]) -> Option<Vec<Plan>> {
    let plans = match shape(product)? {
        Shape::PostgresJson => postgres(rows)?,
        Shape::SqliteRows => sqlite(rows),
    };
    (!plans.is_empty()).then_some(plans)
}

/// The one cell PostgreSQL answers with, which holds `[{"Plan": {…}}]`.
fn postgres(rows: &[Vec<String>]) -> Option<Vec<Plan>> {
    let cell = rows.first()?.first()?;
    let document: Value = serde_json::from_str(cell).ok()?;
    // The array is the outer shape of every `FORMAT JSON` answer, and it holds
    // one element per statement — of which there is one, because this is asked of
    // one statement at a time.
    let plans = document
        .as_array()?
        .iter()
        .filter_map(|entry| entry.get("Plan"))
        .filter_map(|node| pg_node(node.as_object()?))
        .collect();
    Some(plans)
}

/// The keys `pg_node` reads into fields of its own, and which would otherwise be
/// repeated in `detail` as lines saying what the row above already shows.
///
/// `Parent Relationship` is here for a different reason than the rest: it says
/// which input of the parent this is, which the tree itself already says by where
/// the node sits.
const PG_STRUCTURAL: &[&str] = &[
    "Node Type",
    "Plans",
    "Plan Rows",
    "Total Cost",
    "Parent Relationship",
];

/// One node of PostgreSQL's document, and everything under it.
fn pg_node(node: &Map<String, Value>) -> Option<Plan> {
    let label = node.get("Node Type")?.as_str()?.to_string();
    let cost = node.get("Total Cost").and_then(Value::as_f64);
    let children: Vec<Plan> = node
        .get("Plans")
        .and_then(Value::as_array)
        .map(|kids| {
            kids.iter()
                .filter_map(|k| pg_node(k.as_object()?))
                .collect()
        })
        .unwrap_or_default();
    Some(Plan {
        label,
        detail: node
            .iter()
            .filter(|(key, value)| !PG_STRUCTURAL.contains(&key.as_str()) && worth_showing(value))
            .map(|(key, value)| format!("{key}: {}", flatten(value)))
            .collect(),
        rows: node.get("Plan Rows").and_then(Value::as_f64),
        cost,
        self_cost: self_cost(cost, &children),
        children,
    })
}

/// Whether a property says something about this node.
///
/// `false` is the server listing an option this step did not take, and it prints
/// those for every node whether or not the option was ever in question —
/// `Parallel Aware`, `Async Capable`, `Inner Unique`. Six such lines under every
/// step bury the one line that says what the step actually did. A `true` is kept,
/// because that one is news.
fn worth_showing(value: &Value) -> bool {
    match value {
        Value::Bool(false) | Value::Null => false,
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

/// A property value on one line.
///
/// Arrays are joined because PostgreSQL uses them for lists of expressions — sort
/// keys, group keys — where each element is already a phrase. Anything else is
/// written as the JSON it is: unreadable is better than absent, and the shapes
/// that land here are the rare nested ones a reader would otherwise never learn
/// the server had sent.
fn flatten(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(flatten).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// What this step adds to the cost of the steps feeding it. See [`Plan::self_cost`].
fn self_cost(cost: Option<f64>, children: &[Plan]) -> Option<f64> {
    let below: f64 = children.iter().filter_map(|c| c.cost).sum();
    cost.map(|total| (total - below).max(0.0))
}

/// SQLite's four columns, in the order it returns them: id, parent, notused,
/// detail.
///
/// Read by position rather than by name because that is what the caller has —
/// the rows arrive as cells, and the column names are the server's, not a promise
/// this build can hold anybody to. A row that is short or whose ids are not
/// numbers is skipped rather than guessed at.
fn sqlite(rows: &[Vec<String>]) -> Vec<Plan> {
    // Every row names a parent that appeared earlier, so one pass building a
    // flat list and one pass hanging children off it is enough — no ordering
    // work, and a row naming a parent that is not there stays a root rather than
    // disappearing.
    let mut ids: Vec<i64> = Vec::new();
    let mut parents: Vec<i64> = Vec::new();
    let mut nodes: Vec<Plan> = Vec::new();
    for row in rows {
        let (Some(id), Some(parent), Some(detail)) = (row.first(), row.get(1), row.get(3)) else {
            continue;
        };
        let (Ok(id), Ok(parent)) = (id.trim().parse::<i64>(), parent.trim().parse::<i64>()) else {
            continue;
        };
        ids.push(id);
        parents.push(parent);
        nodes.push(Plan {
            label: detail.clone(),
            detail: Vec::new(),
            // SQLite's planner publishes no estimate here at all. `None` rather
            // than a zero, which would draw a bar saying this step is free.
            rows: None,
            cost: None,
            self_cost: None,
            children: Vec::new(),
        });
    }

    // Built from the end so that a node is finished before the parent that takes
    // it: every parent appears before its children, so walking backwards means
    // whatever is removed here has already collected its own.
    let mut roots: Vec<Plan> = Vec::new();
    while let Some(node) = nodes.pop() {
        let parent = parents.pop().unwrap_or(0);
        ids.pop();
        match ids.iter().position(|id| *id == parent) {
            Some(at) => nodes[at].children.insert(0, node),
            None => roots.insert(0, node),
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document PostgreSQL 17 answered with for a grouped join, trimmed of
    /// nothing. Kept whole rather than reduced to the keys under test, because
    /// what this module has to survive is the shape a server actually sends.
    const PG_PLAN: &str = r#"[
      {
        "Plan": {
          "Node Type": "Limit",
          "Parallel Aware": false,
          "Async Capable": false,
          "Startup Cost": 1051.92,
          "Total Cost": 1051.94,
          "Plan Rows": 10,
          "Plan Width": 18,
          "Plans": [
            {
              "Node Type": "Aggregate",
              "Strategy": "Hashed",
              "Parent Relationship": "Outer",
              "Parallel Aware": false,
              "Startup Cost": 893.87,
              "Total Cost": 943.87,
              "Plan Rows": 5000,
              "Plan Width": 18,
              "Group Key": ["w.name"],
              "Plans": [
                {
                  "Node Type": "Seq Scan",
                  "Parent Relationship": "Outer",
                  "Relation Name": "bench_child",
                  "Alias": "c",
                  "Startup Cost": 0.00,
                  "Total Cost": 100.00,
                  "Plan Rows": 5000,
                  "Plan Width": 4,
                  "Filter": "(int_val > 100)"
                }
              ]
            }
          ]
        }
      }
    ]"#;

    fn pg(document: &str) -> Vec<Plan> {
        read("PostgreSQL", &[vec![document.to_string()]]).expect("a plan")
    }

    fn rows(cells: &[[&str; 4]]) -> Vec<Vec<String>> {
        cells
            .iter()
            .map(|row| row.iter().map(|c| c.to_string()).collect())
            .collect()
    }

    #[test]
    fn a_postgresql_document_becomes_the_tree_the_server_described() {
        let plans = pg(PG_PLAN);
        assert_eq!(plans.len(), 1);
        let root = &plans[0];
        assert_eq!(root.label, "Limit");
        assert_eq!(root.rows, Some(10.0));
        assert_eq!(root.cost, Some(1051.94));
        let aggregate = &root.children[0];
        assert_eq!(aggregate.label, "Aggregate");
        assert_eq!(aggregate.children[0].label, "Seq Scan");
        assert!(aggregate.children[0].children.is_empty());
    }

    /// The cost a step adds is what the bar beside it means, and the number the
    /// server prints is not it.
    ///
    /// Asserted on the aggregate rather than on the root because the root's own
    /// arithmetic — 1051.94 less its one child's 943.87 — is the same subtraction
    /// one level up, and a version of this that only ever subtracted at the top
    /// would pass on that one alone.
    #[test]
    fn the_cost_a_step_adds_is_its_own_and_not_everything_below_it() {
        let root = &pg(PG_PLAN)[0];
        let aggregate = &root.children[0];
        assert_eq!(aggregate.cost, Some(943.87));
        assert_eq!(aggregate.self_cost, Some(843.87));
        // The leaf has nothing under it, so the two are the same number and it is
        // not the subtraction being skipped.
        assert_eq!(aggregate.children[0].self_cost, Some(100.0));
    }

    /// A nested loop charges its inner side by the number of loops, so the
    /// children can be dearer than the parent that runs them.
    #[test]
    fn a_step_that_costs_less_than_its_children_adds_nothing_rather_than_less() {
        let document = r#"[{"Plan": {"Node Type": "Nested Loop", "Total Cost": 10.0,
            "Plans": [{"Node Type": "Seq Scan", "Total Cost": 400.0}]}}]"#;
        assert_eq!(pg(document)[0].self_cost, Some(0.0));
    }

    /// Lines saying "no" about an option that was never in question are what a
    /// reader is scrolling past to reach the one line that says what happened.
    #[test]
    fn a_step_lists_what_it_did_and_not_what_it_did_not() {
        let root = &pg(PG_PLAN)[0];
        assert!(
            !root.detail.iter().any(|line| line.contains("false")),
            "kept a line that says no: {:?}",
            root.detail
        );
        let scan = &root.children[0].children[0];
        assert!(
            scan.detail
                .contains(&"Relation Name: bench_child".to_string()),
            "{:?}",
            scan.detail
        );
        assert!(
            scan.detail.contains(&"Filter: (int_val > 100)".to_string()),
            "{:?}",
            scan.detail
        );
    }

    /// The numbers the fields carry must not also arrive as prose, or every step
    /// says its cost twice and disagrees with itself the day a field changes.
    #[test]
    fn what_a_field_carries_is_not_repeated_as_a_line() {
        let root = &pg(PG_PLAN)[0];
        for line in &root.detail {
            for repeated in ["Node Type", "Total Cost", "Plan Rows", "Plans"] {
                assert!(
                    !line.starts_with(repeated),
                    "{repeated} is a field and a line: {line}"
                );
            }
        }
        // `Startup Cost` is not a field and must therefore still be a line: this
        // is a filter of named keys, not of everything that looks like a cost.
        assert!(
            root.detail.iter().any(|l| l.starts_with("Startup Cost")),
            "{:?}",
            root.detail
        );
    }

    /// A list of expressions is one fact about the step, not one per element.
    #[test]
    fn a_list_of_expressions_reads_as_one_line() {
        let root = &pg(PG_PLAN)[0];
        assert!(
            root.children[0]
                .detail
                .contains(&"Group Key: w.name".to_string()),
            "{:?}",
            root.children[0].detail
        );
    }

    #[test]
    fn sqlite_rows_hang_off_the_steps_they_name() {
        // The answer to a `UNION ALL` whose left arm holds a subquery, exactly as
        // SQLite 3 returned it.
        let plans = read(
            "SQLite",
            &rows(&[
                ["1", "0", "0", "MERGE (UNION ALL)"],
                ["3", "1", "0", "LEFT"],
                [
                    "6",
                    "3",
                    "91",
                    "SEARCH w USING INTEGER PRIMARY KEY (rowid=?)",
                ],
                ["10", "3", "0", "LIST SUBQUERY 1"],
                ["13", "10", "216", "SCAN c"],
                ["43", "1", "0", "RIGHT"],
                ["46", "43", "216", "SCAN c"],
            ]),
        )
        .expect("a plan");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].label, "MERGE (UNION ALL)");
        let arms = &plans[0].children;
        assert_eq!(arms.len(), 2);
        assert_eq!(arms[0].label, "LEFT");
        assert_eq!(arms[1].label, "RIGHT");
        // Order within a parent is the order the server listed them in: the plan
        // reads top to bottom the way its own shell prints it.
        assert_eq!(
            arms[0]
                .children
                .iter()
                .map(|c| &c.label)
                .collect::<Vec<_>>(),
            vec![
                "SEARCH w USING INTEGER PRIMARY KEY (rowid=?)",
                "LIST SUBQUERY 1"
            ]
        );
        assert_eq!(arms[0].children[1].children[0].label, "SCAN c");
    }

    /// Several top-level steps is SQLite's ordinary answer, not a broken one.
    #[test]
    fn a_statement_with_several_top_level_steps_keeps_all_of_them() {
        let plans = read(
            "SQLite",
            &rows(&[
                ["9", "0", "210", "SCAN c USING COVERING INDEX c_parent"],
                [
                    "11",
                    "0",
                    "45",
                    "SEARCH w USING INTEGER PRIMARY KEY (rowid=?)",
                ],
                ["16", "0", "0", "USE TEMP B-TREE FOR GROUP BY"],
            ]),
        )
        .expect("a plan");
        assert_eq!(plans.len(), 3);
    }

    /// The table is keyed by product because the dialect is the wrong key: these
    /// three arrive through the drivers of the two that answer.
    #[test]
    fn a_product_that_rides_another_products_driver_is_not_asked() {
        for product in ["CockroachDB", "GreptimeDB", "TiDB", "MySQL", "MariaDB"] {
            assert_eq!(prefix(product), None, "{product} was asked for a plan");
            assert_eq!(read(product, &[vec!["[]".to_string()]]), None);
        }
        assert_eq!(prefix("PostgreSQL"), Some("EXPLAIN (FORMAT JSON)"));
        assert_eq!(prefix("SQLite"), Some("EXPLAIN QUERY PLAN"));
    }

    /// A prefix is joined to the statement with exactly one space by its caller,
    /// so a padded or empty one produces SQL the server rejects.
    #[test]
    fn every_prefix_is_the_words_and_nothing_else() {
        for product in ["PostgreSQL", "SQLite"] {
            let prefix = prefix(product).expect("a prefix");
            assert!(!prefix.is_empty());
            assert_eq!(prefix.trim(), prefix, "{product} has a padded prefix");
        }
    }

    /// Nothing to draw is not the same as a tree with nothing in it: a caller
    /// that got `Some(vec![])` would clear the grid and show an empty pane.
    #[test]
    fn an_answer_that_is_not_a_plan_is_no_plan_at_all() {
        assert_eq!(read("PostgreSQL", &[]), None);
        assert_eq!(read("PostgreSQL", &[vec!["not json".to_string()]]), None);
        assert_eq!(read("PostgreSQL", &[vec!["[]".to_string()]]), None);
        assert_eq!(read("SQLite", &[]), None);
        // Rows that are not the four SQLite sends. Skipped one by one, which
        // leaves nothing, which is no plan.
        assert_eq!(read("SQLite", &[vec!["only one cell".to_string()]]), None);
    }
}
