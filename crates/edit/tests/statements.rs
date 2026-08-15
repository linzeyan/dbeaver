//! What a grid's changes turn into, against a catalog that answers from memory.
//!
//! A fake driver rather than a server, because every question here is about the
//! statement and none is about the database: whether the row is named by its key
//! alone, whether a value is quoted the way its column needs, and whether a
//! change this crate cannot make safely is refused instead of sent. The
//! statements are then run against a real server by the FFI's conformance
//! harness, which is where "this text is valid SQL" belongs.

use dbconn::{
    Browse, ColumnInfo, ConstraintInfo, Cursor, DbResult, Driver, IndexInfo, RelationInfo,
    RelationshipInfo, ResultStream, SchemaInfo, TriggerInfo, TxStep,
};
use dbedit::Edits;

/// Two tables: one with a compound key, one with none at all.
struct Fake;

#[async_trait::async_trait]
impl Driver for Fake {
    async fn columns(&self, _: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        let described: &[(&str, &str, bool)] = match relation {
            "lines" => &[
                ("order_id", "int4", true),
                ("line_no", "int2", true),
                ("sku", "text", false),
                ("qty", "numeric(9, 2)", false),
                ("shipped_at", "timestamp", false),
                ("note", "text", false),
            ],
            "no_key" => &[("label", "text", false)],
            _ => &[],
        };
        Ok(described
            .iter()
            .enumerate()
            .map(|(i, (name, data_type, key))| ColumnInfo {
                name: name.to_string(),
                data_type: data_type.to_string(),
                nullable: !key,
                position: i as i32 + 1,
                is_primary_key: *key,
                default_value: None,
                computed: None,
            })
            .collect())
    }

    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>> {
        unreachable!("an edit names the relation it changes")
    }
    async fn relations(&self, _: &str) -> DbResult<Vec<RelationInfo>> {
        unreachable!("an edit names the relation it changes")
    }
    async fn definition(&self, _: &str, _: &str) -> DbResult<Option<String>> {
        unreachable!("nothing here reads a view")
    }
    async fn indexes(&self, _: &str, _: &str) -> DbResult<Vec<IndexInfo>> {
        unreachable!("the key comes from the columns")
    }
    async fn foreign_keys(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("an edit is one relation's business")
    }
    async fn referenced_by(&self, _: &str, _: &str) -> DbResult<Vec<RelationshipInfo>> {
        unreachable!("an edit is one relation's business")
    }
    async fn constraints(&self, _: &str, _: &str) -> DbResult<Vec<ConstraintInfo>> {
        unreachable!("the server enforces its own constraints")
    }
    async fn triggers(&self, _: &str, _: &str) -> DbResult<Vec<TriggerInfo>> {
        unreachable!("the server fires its own triggers")
    }

    fn browse(&self, _: &Browse<'_>) -> String {
        unreachable!("this crate writes statements of its own")
    }
    async fn query(&self, _: &str, _: usize) -> DbResult<Box<dyn ResultStream>> {
        unreachable!("this crate writes statements and runs none")
    }
    async fn cursor(&self, _: &str, _: usize) -> DbResult<Box<dyn Cursor>> {
        unreachable!("this crate writes statements and runs none")
    }
    async fn cancel(&self) -> DbResult<()> {
        unreachable!("nothing here reaches a server to cancel")
    }
    fn transactional(&self) -> bool {
        false
    }
    async fn transaction(&self, _: &TxStep) -> DbResult<()> {
        unreachable!("the caller owns the transaction these statements go in")
    }
}

/// The statements `json` produces, insisting they were produced.
async fn written(json: &str) -> Vec<String> {
    let edits: Edits = serde_json::from_str(json).expect("the edits should parse");
    dbedit::statements(&Fake, &dbsql::POSTGRES, &edits)
        .await
        .expect("the statements should be writable")
}

/// Why `json` was refused, insisting it was.
async fn refused(json: &str) -> String {
    let edits: Edits = serde_json::from_str(json).expect("the edits should parse");
    dbedit::statements(&Fake, &dbsql::POSTGRES, &edits)
        .await
        .expect_err("this should not have been written")
        .to_string()
}

#[tokio::test]
async fn a_changed_cell_becomes_an_update_naming_one_row() {
    let statements = written(
        r#"{"schema":"public","relation":"lines","updates":[
             {"key":[{"column":"order_id","value":"12"},{"column":"line_no","value":"3"}],
              "set":[{"column":"sku","value":"ABC-1"}]}]}"#,
    )
    .await;
    assert_eq!(
        statements,
        ["UPDATE public.lines SET sku = 'ABC-1' WHERE order_id = 12 AND line_no = 3"]
    );
}

#[tokio::test]
async fn a_number_is_written_bare_and_everything_else_is_quoted() {
    let statements = written(
        r#"{"schema":"public","relation":"lines","updates":[
             {"key":[{"column":"order_id","value":"12"},{"column":"line_no","value":"3"}],
              "set":[{"column":"qty","value":"2.50"},
                     {"column":"shipped_at","value":"2026-08-14 09:30:00"},
                     {"column":"note","value":null}]}]}"#,
    )
    .await;
    // The timestamp is quoted and left exactly as typed: the server knows how to
    // read its own dates, and a client that reformatted one would be guessing at
    // a locale it was not told.
    assert_eq!(
        statements[0],
        "UPDATE public.lines SET qty = 2.50, shipped_at = '2026-08-14 09:30:00', note = NULL \
         WHERE order_id = 12 AND line_no = 3"
    );
}

#[tokio::test]
async fn a_quote_in_a_value_is_doubled_rather_than_escaped() {
    let statements = written(
        r#"{"schema":"public","relation":"lines","updates":[
             {"key":[{"column":"order_id","value":"1"},{"column":"line_no","value":"1"}],
              "set":[{"column":"note","value":"it's fine"}]}]}"#,
    )
    .await;
    assert!(
        statements[0].contains("note = 'it''s fine'"),
        "{statements:?}"
    );
}

/// A backslash is data on PostgreSQL and an escape on MySQL, and the value has
/// to come back the same on both.
#[tokio::test]
async fn a_backslash_is_doubled_only_where_it_escapes() {
    let json = r#"{"schema":"public","relation":"lines","updates":[
             {"key":[{"column":"order_id","value":"1"},{"column":"line_no","value":"1"}],
              "set":[{"column":"note","value":"C:\\temp"}]}]}"#;
    let edits: Edits = serde_json::from_str(json).unwrap();

    let postgres = dbedit::statements(&Fake, &dbsql::POSTGRES, &edits)
        .await
        .unwrap();
    assert!(postgres[0].contains(r"note = 'C:\temp'"), "{postgres:?}");

    let mysql = dbedit::statements(&Fake, &dbsql::MYSQL, &edits)
        .await
        .unwrap();
    assert!(mysql[0].contains(r"note = 'C:\\temp'"), "{mysql:?}");
}

#[tokio::test]
async fn an_inserted_row_names_only_the_columns_it_was_given() {
    let statements = written(
        r#"{"schema":"public","relation":"lines","inserts":[
             {"set":[{"column":"order_id","value":"20"},
                     {"column":"line_no","value":"1"},
                     {"column":"sku","value":"NEW"}]}]}"#,
    )
    .await;
    // Not every column: a table's defaults are the reason a row can be inserted
    // without filling one in, and listing them all as NULL would override them.
    assert_eq!(
        statements,
        ["INSERT INTO public.lines (order_id, line_no, sku) VALUES (20, 1, 'NEW')"]
    );
}

/// A new row nobody typed into is a row of the table's own defaults, and the
/// three databases here spell that three different ways.
///
/// The front end decides whether such a row may be sent at all — its setting
/// defaults to refusing it there, by name — but once it arrives the statement
/// has to be the one this database reads. `DEFAULT VALUES` sent to MySQL is a
/// syntax error, and empty parentheses sent to PostgreSQL are another.
#[tokio::test]
async fn a_row_of_nothing_takes_each_database_at_its_own_word() {
    let edits: Edits =
        serde_json::from_str(r#"{"schema":"public","relation":"lines","inserts":[{"set":[]}]}"#)
            .unwrap();

    let postgres = dbedit::statements(&Fake, &dbsql::POSTGRES, &edits)
        .await
        .unwrap();
    assert_eq!(postgres, ["INSERT INTO public.lines DEFAULT VALUES"]);

    let mysql = dbedit::statements(&Fake, &dbsql::MYSQL, &edits)
        .await
        .unwrap();
    // Backticks because `lines` is one of MySQL's own keywords, which is the
    // same quoting every other statement in this file gets there.
    assert_eq!(mysql, ["INSERT INTO public.`lines` () VALUES ()"]);

    // Named rather than approximated. A database with no way to say this gets a
    // refusal carrying both halves of the reason — which table, and which
    // database — because a statement invented here would fail at the server
    // with a syntax error that points at nothing the user did.
    let why = dbedit::statements(&Fake, &dbsql::CLICKHOUSE, &edits)
        .await
        .expect_err("clickhouse has no spelling for a row of defaults")
        .to_string();
    assert!(why.contains("public.lines"), "{why}");
    assert!(why.contains("clickhouse"), "{why}");
}

#[tokio::test]
async fn a_deleted_row_is_named_the_same_way_an_updated_one_is() {
    let statements = written(
        r#"{"schema":"public","relation":"lines","deletes":[
             {"key":[{"column":"order_id","value":"12"},{"column":"line_no","value":"3"}]}]}"#,
    )
    .await;
    assert_eq!(
        statements,
        ["DELETE FROM public.lines WHERE order_id = 12 AND line_no = 3"]
    );
}

/// Deletes go last, so a row an update still needs is not taken from under it
/// and a key an insert reuses is free by the time it is reused.
#[tokio::test]
async fn a_batch_is_ordered_so_that_one_statement_cannot_undo_the_next() {
    let statements = written(
        r#"{"schema":"public","relation":"lines",
            "deletes":[{"key":[{"column":"order_id","value":"1"},{"column":"line_no","value":"1"}]}],
            "inserts":[{"set":[{"column":"order_id","value":"2"},{"column":"line_no","value":"1"}]}],
            "updates":[{"key":[{"column":"order_id","value":"3"},{"column":"line_no","value":"1"}],
                        "set":[{"column":"sku","value":"X"}]}]}"#,
    )
    .await;
    assert!(statements[0].starts_with("UPDATE"), "{statements:?}");
    assert!(statements[1].starts_with("INSERT"), "{statements:?}");
    assert!(statements[2].starts_with("DELETE"), "{statements:?}");
}

#[tokio::test]
async fn a_relation_with_no_primary_key_cannot_be_edited() {
    // The whole reason this crate refuses rather than matching on every column:
    // a `WHERE` clause of every column is one that can quietly be false for the
    // row the user was looking at, and then the edit does nothing at all.
    let why = refused(
        r#"{"schema":"public","relation":"no_key","updates":[
             {"key":[{"column":"label","value":"a"}],
              "set":[{"column":"label","value":"b"}]}]}"#,
    )
    .await;
    assert!(why.contains("no primary key"), "{why}");
}

#[tokio::test]
async fn a_key_that_is_not_the_whole_key_is_refused() {
    let why = refused(
        r#"{"schema":"public","relation":"lines","updates":[
             {"key":[{"column":"order_id","value":"12"}],
              "set":[{"column":"sku","value":"ABC"}]}]}"#,
    )
    .await;
    assert!(why.contains("line_no"), "{why}");
}

#[tokio::test]
async fn a_column_the_relation_does_not_have_is_refused() {
    let why = refused(
        r#"{"schema":"public","relation":"lines","updates":[
             {"key":[{"column":"order_id","value":"1"},{"column":"line_no","value":"1"}],
              "set":[{"column":"quantity","value":"1"}]}]}"#,
    )
    .await;
    assert!(why.contains("quantity"), "{why}");
}

#[tokio::test]
async fn text_that_is_not_a_number_never_reaches_a_numeric_column() {
    // The one place quoting would be worse than refusing: `qty = 2; DROP TABLE
    // lines` typed into a numeric cell is either a syntax error or a statement,
    // and the difference is one pair of quotes this crate would rather not be
    // deciding about under pressure.
    let why = refused(
        r#"{"schema":"public","relation":"lines","updates":[
             {"key":[{"column":"order_id","value":"1"},{"column":"line_no","value":"1"}],
              "set":[{"column":"qty","value":"2; DROP TABLE lines"}]}]}"#,
    )
    .await;
    assert!(why.contains("not a number"), "{why}");
}

#[tokio::test]
async fn a_name_that_needs_quoting_gets_it() {
    // The relation, not the column: a schema or table whose name is a keyword or
    // is not lower case has to survive being written into the statement.
    let edits: Edits = serde_json::from_str(
        r#"{"schema":"Order Data","relation":"lines","deletes":[
             {"key":[{"column":"order_id","value":"1"},{"column":"line_no","value":"1"}]}]}"#,
    )
    .unwrap();
    let statements = dbedit::statements(&Fake, &dbsql::POSTGRES, &edits)
        .await
        .unwrap();
    assert!(
        statements[0].starts_with(r#"DELETE FROM "Order Data".lines"#),
        "{statements:?}"
    );
}
