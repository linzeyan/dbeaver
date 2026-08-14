//! Completion against input that is always broken, because that is the input.
//!
//! Every case below is a statement the server would refuse, which is not a
//! contrivance — it is the only kind of statement that exists while somebody is
//! typing. A parser that needs valid SQL answers correctly on none of them.
//!
//! `▮` marks the caret in the source of each case and is removed before the
//! text is read, so the cases stay legible.

use dbsql::{DUCKDB, Dialect, Expect, MSSQL, MYSQL, POSTGRES};

/// The completion at the `▮` in `marked`.
fn at(marked: &str, dialect: &Dialect) -> dbsql::Completion {
    let caret = marked.chars().position(|c| c == '▮').expect("no caret") as u32;
    let text = marked.replace('▮', "");
    dbsql::complete(&text, caret, dialect)
}

/// The relations in scope, by the name they answer to.
fn visible(marked: &str, dialect: &Dialect) -> Vec<String> {
    at(marked, dialect)
        .sources
        .iter()
        .map(|s| s.handle().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The question, not the answer
// ---------------------------------------------------------------------------

#[test]
fn the_start_of_a_buffer_expects_a_verb() {
    assert_eq!(at("▮", &POSTGRES).expect, Expect::Statement);
    assert_eq!(at("SEL▮", &POSTGRES).expect, Expect::Statement);
    assert_eq!(at("SEL▮", &POSTGRES).prefix, "SEL");
}

#[test]
fn a_name_after_from_is_a_relation() {
    assert_eq!(
        at("SELECT * FROM ▮", &POSTGRES).expect,
        Expect::Relation { schema: None }
    );
    assert_eq!(
        at("SELECT * FROM ord▮", &POSTGRES).expect,
        Expect::Relation { schema: None }
    );
    assert_eq!(at("SELECT * FROM ord▮", &POSTGRES).prefix, "ord");
}

#[test]
fn a_name_in_an_expression_is_a_column() {
    for marked in [
        "SELECT ▮ FROM orders",
        "SELECT id, ▮ FROM orders",
        "SELECT * FROM orders WHERE ▮",
        "SELECT * FROM orders GROUP BY ▮",
        "SELECT * FROM orders ORDER BY ▮",
    ] {
        assert_eq!(
            at(marked, &POSTGRES).expect,
            Expect::Column { qualifier: None },
            "for {marked}"
        );
    }
}

#[test]
fn a_qualifier_that_names_a_relation_in_scope_asks_for_its_columns() {
    let c = at("SELECT o.▮ FROM orders o", &POSTGRES);
    assert_eq!(
        c.expect,
        Expect::Column {
            qualifier: Some("o".to_string())
        }
    );
    // And with the relation's own name rather than an alias.
    let c = at("SELECT orders.▮ FROM orders", &POSTGRES);
    assert_eq!(
        c.expect,
        Expect::Column {
            qualifier: Some("orders".to_string())
        }
    );
}

#[test]
fn a_qualifier_that_names_nothing_in_scope_asks_for_a_relation() {
    // `public.` and `o.` are the same three characters. What separates them is
    // the clause: in a FROM list a qualified name is schema-then-relation.
    // Offering columns here would offer the columns of nothing.
    let c = at("SELECT * FROM public.▮", &POSTGRES);
    assert_eq!(
        c.expect,
        Expect::Relation {
            schema: Some("public".to_string())
        }
    );
}

#[test]
fn there_is_nothing_to_offer_inside_a_literal_or_a_comment() {
    for marked in [
        "SELECT 'a▮b' FROM t",
        "SELECT * FROM t -- a ▮",
        "SELECT /* ▮ */ 1",
        "SELECT $$ ▮ $$",
    ] {
        assert_eq!(
            at(marked, &POSTGRES).expect,
            Expect::Nothing,
            "for {marked}"
        );
    }
}

// ---------------------------------------------------------------------------
// The answer is written after the caret
// ---------------------------------------------------------------------------

#[test]
fn a_relation_named_after_the_caret_is_still_in_scope() {
    // The case the whole design exists for. Nothing before the caret says what
    // is being selected from, and a parser that stopped at the caret — or that
    // needed the statement to be valid — would have nothing to offer.
    assert_eq!(visible("SELECT ▮ FROM orders o", &POSTGRES), ["o"]);
    assert_eq!(
        visible(
            "SELECT ▮ FROM orders o JOIN customers c ON c.id = o.customer",
            &POSTGRES
        ),
        ["o", "c"]
    );
}

#[test]
fn a_relation_is_known_by_the_name_it_answers_to() {
    assert_eq!(visible("SELECT ▮ FROM orders", &POSTGRES), ["orders"]);
    assert_eq!(visible("SELECT ▮ FROM orders AS o", &POSTGRES), ["o"]);
    assert_eq!(visible("SELECT ▮ FROM sales.orders", &POSTGRES), ["orders"]);
    let c = at("SELECT ▮ FROM sales.orders", &POSTGRES);
    assert_eq!(c.sources[0].schema.as_deref(), Some("sales"));
}

#[test]
fn a_quoted_name_arrives_without_its_quotes() {
    // The catalog holds `Order`, not `"Order"`, and asking it about the latter
    // finds nothing.
    assert_eq!(
        visible(r#"SELECT ▮ FROM "Order Lines""#, &POSTGRES),
        ["Order Lines"]
    );
    assert_eq!(
        visible("SELECT ▮ FROM `weird name`", &MYSQL),
        ["weird name"]
    );
    assert_eq!(
        visible("SELECT ▮ FROM [weird name]", &MSSQL),
        ["weird name"]
    );
}

// ---------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------

#[test]
fn a_subquery_sees_its_own_relations_first_and_the_outer_ones_too() {
    // Correlated subqueries are why the outer ones stay visible: `o` really is
    // nameable from inside.
    let visible = visible(
        "SELECT * FROM orders o WHERE EXISTS (SELECT ▮ FROM lines l WHERE l.order = o.id)",
        &POSTGRES,
    );
    assert_eq!(visible, ["l", "o"]);
}

#[test]
fn a_relation_inside_a_subquery_is_not_visible_outside_it() {
    let visible = visible(
        "SELECT ▮ FROM (SELECT id FROM lines) x, orders o",
        &POSTGRES,
    );
    assert!(visible.contains(&"o".to_string()));
    assert!(visible.contains(&"x".to_string()));
    assert!(!visible.contains(&"lines".to_string()), "got {visible:?}");
}

#[test]
fn a_common_table_expression_is_a_name_the_statement_can_select_from() {
    let c = at(
        "WITH recent AS (SELECT * FROM orders) SELECT ▮ FROM recent r",
        &POSTGRES,
    );
    let names: Vec<&str> = c.sources.iter().map(|s| s.handle()).collect();
    assert!(names.contains(&"r"), "got {names:?}");
    assert!(names.contains(&"recent"), "got {names:?}");
    // A CTE is not something to ask the catalog about.
    assert!(
        c.sources
            .iter()
            .find(|s| s.name == "recent")
            .unwrap()
            .derived
    );
}

// ---------------------------------------------------------------------------
// Input that is broken in the ways typing breaks it
// ---------------------------------------------------------------------------

#[test]
fn an_unclosed_subquery_still_reports_what_is_inside_it() {
    // Half a statement, which is what a statement is for most of its life.
    assert_eq!(
        visible(
            "SELECT * FROM orders WHERE id IN (SELECT ▮ FROM lines",
            &POSTGRES
        ),
        ["lines", "orders"]
    );
}

#[test]
fn a_comma_with_nothing_after_it_yet_does_not_lose_the_relations_before_it() {
    assert_eq!(
        visible("SELECT a, ▮ FROM orders o, customers c", &POSTGRES),
        ["o", "c"]
    );
    assert_eq!(visible("SELECT * FROM orders o, ▮", &POSTGRES), ["o"]);
}

#[test]
fn a_statement_with_no_from_clause_yet_offers_no_relations_rather_than_failing() {
    let c = at("SELECT ▮", &POSTGRES);
    assert_eq!(c.expect, Expect::Column { qualifier: None });
    assert!(c.sources.is_empty());
}

#[test]
fn only_the_statement_the_caret_is_in_contributes() {
    let visible = visible("SELECT * FROM elsewhere; SELECT ▮ FROM orders o", &POSTGRES);
    assert_eq!(visible, ["o"]);
}

// ---------------------------------------------------------------------------
// Dialects reach this only through the tokens
// ---------------------------------------------------------------------------

#[test]
fn a_word_that_is_a_keyword_in_one_dialect_and_a_name_in_another_is_read_as_each() {
    // `QUALIFY` is DuckDB's, and PostgreSQL has no such word — so in PostgreSQL
    // it is an ordinary alias and in DuckDB it is the next clause. Neither the
    // parser nor completion names a database to know that.
    assert_eq!(visible("SELECT ▮ FROM t qualify", &POSTGRES), ["qualify"]);
    assert_eq!(visible("SELECT ▮ FROM t qualify", &DUCKDB), ["t"]);
}

#[test]
fn each_dialect_reads_its_own_quoting_when_finding_a_relation() {
    // Backticks are identifiers in MySQL and nothing in PostgreSQL; a double
    // quote is an identifier in PostgreSQL and a string in MySQL.
    assert_eq!(visible("SELECT ▮ FROM `t`", &MYSQL), ["t"]);
    assert_eq!(at("SELECT ▮ FROM \"t\"", &MYSQL).sources.len(), 0);
    assert_eq!(visible("SELECT ▮ FROM \"t\"", &POSTGRES), ["t"]);
}

// ---------------------------------------------------------------------------
// Scale
// ---------------------------------------------------------------------------

#[test]
fn a_long_script_is_answered_from_the_statement_the_caret_is_in() {
    // Two thousand statements ahead of the caret, which is an ordinary size for
    // a migration file somebody has open. The answer must not depend on how
    // much is above it, and the work must not either — completion runs on every
    // keystroke, and a scan per question would be four scans of a script that
    // has not changed between them.
    let mut script = String::new();
    for i in 0..2_000 {
        script.push_str(&format!(
            "-- statement {i}\nINSERT INTO archive.rows (id, label) VALUES ({i}, 'row-{i}');\n"
        ));
    }
    let caret = script.chars().count() as u32;
    script.push_str("SELECT  FROM orders o JOIN customers c ON c.id = o.customer");
    let caret = caret + "SELECT ".chars().count() as u32;

    let started = std::time::Instant::now();
    let c = dbsql::complete(&script, caret, &POSTGRES);
    let took = started.elapsed();

    assert_eq!(c.expect, Expect::Column { qualifier: None });
    let names: Vec<&str> = c.sources.iter().map(|s| s.handle()).collect();
    assert_eq!(names, ["o", "c"]);
    // A bound loose enough that a busy machine cannot fail it, and tight enough
    // that reintroducing a scan per question would.
    assert!(took.as_millis() < 500, "completion took {took:?}");
}
