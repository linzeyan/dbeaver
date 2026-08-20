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
    RelationshipInfo, ResultStream, SchemaInfo, TriggerInfo, TxStep, UniqueKeyInfo,
};
use dbedit::Edits;

/// One column of one table: name, declared type, whether it is in the primary
/// key, whether it can be null.
///
/// The last two are separate because the whole of the unique-key rule lives in
/// the difference: a table with no primary key and a NOT NULL unique column can
/// name a row, and the same table with that column nullable cannot.
type Column = (&'static str, &'static str, bool, bool);

/// Six tables, each the smallest example of one thing a key can be.
struct Fake;

#[async_trait::async_trait]
impl Driver for Fake {
    async fn columns(&self, _: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        let described: &[Column] = match relation {
            "lines" => &[
                ("order_id", "int4", true, false),
                ("line_no", "int2", true, false),
                ("sku", "text", false, true),
                ("qty", "numeric(9, 2)", false, true),
                ("shipped_at", "timestamp", false, true),
                ("note", "text", false, true),
            ],
            "no_key" => &[("label", "text", false, true)],
            // No primary key, one unique key over a column that cannot be null.
            "sessions" => &[
                ("token", "text", false, false),
                ("note", "text", false, true),
            ],
            // The same shape with the column nullable, which is the case the
            // rule has to refuse.
            "invitations" => &[
                ("email", "text", false, true),
                ("note", "text", false, true),
            ],
            // Three unique keys, so that one has to be chosen.
            "memberships" => &[
                ("tenant", "text", false, false),
                ("member", "text", false, false),
                ("code", "text", false, false),
                ("alias", "text", false, false),
                ("label", "text", false, true),
            ],
            // A driver contradicting itself: a key over a column the column list
            // does not have.
            "ghost" => &[("label", "text", false, false)],
            _ => &[],
        };
        Ok(described
            .iter()
            .enumerate()
            .map(|(i, (name, data_type, key, nullable))| ColumnInfo {
                name: name.to_string(),
                data_type: data_type.to_string(),
                nullable: *nullable,
                position: i as i32 + 1,
                is_primary_key: *key,
                default_value: None,
                computed: None,
            })
            .collect())
    }

    /// Deliberately not in the order the rule picks from, so that a check which
    /// passes because the first one happened to be right does not exist.
    async fn unique_keys(&self, _: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>> {
        let declared: &[(&str, &[&str])] = match relation {
            "sessions" => &[("sessions_token_key", &["token"])],
            "invitations" => &[("invitations_email_key", &["email"])],
            "memberships" => &[
                ("memberships_tenant_member_key", &["tenant", "member"]),
                ("memberships_code_key", &["code"]),
                ("memberships_alias_key", &["alias"]),
            ],
            "ghost" => &[("ghost_missing_key", &["nowhere"])],
            _ => &[],
        };
        Ok(declared
            .iter()
            .map(|(name, columns)| UniqueKeyInfo {
                name: name.to_string(),
                columns: columns.iter().map(|c| c.to_string()).collect(),
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
        unreachable!("a key is a constraint here, not whatever the planner can use")
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
async fn a_relation_with_nothing_unique_cannot_be_edited() {
    // The whole reason this crate refuses rather than matching on every column:
    // a `WHERE` clause of every column is one that can quietly be false for the
    // row the user was looking at, and then the edit does nothing at all.
    let why = refused(
        r#"{"schema":"public","relation":"no_key","updates":[
             {"key":[{"column":"label","value":"a"}],
              "set":[{"column":"label","value":"b"}]}]}"#,
    )
    .await;
    assert!(why.contains("no primary key or unique key"), "{why}");
}

/// A table with no primary key is editable through its unique key, and the
/// `WHERE` clause is that key.
#[tokio::test]
async fn a_not_null_unique_key_names_a_row_where_there_is_no_primary_key() {
    let statements = written(
        r#"{"schema":"public","relation":"sessions","updates":[
             {"key":[{"column":"token","value":"abc"}],
              "set":[{"column":"note","value":"seen"}]}]}"#,
    )
    .await;
    assert_eq!(
        statements,
        ["UPDATE public.sessions SET note = 'seen' WHERE token = 'abc'"]
    );
}

/// The key's value is the one the row was read with, on a unique key exactly as
/// on a primary one — editing the key column still identifies the row by what it
/// was.
#[tokio::test]
async fn changing_the_unique_key_still_names_the_row_by_its_old_value() {
    let statements = written(
        r#"{"schema":"public","relation":"sessions","updates":[
             {"key":[{"column":"token","value":"old"}],
              "set":[{"column":"token","value":"new"}]}]}"#,
    )
    .await;
    assert_eq!(
        statements,
        ["UPDATE public.sessions SET token = 'new' WHERE token = 'old'"]
    );
}

/// A unique column that can be null is not a key, and the refusal says which
/// constraint it turned down.
///
/// `NULL != NULL` is the whole of it: the row holding NULL is matched by no
/// `WHERE token = …`, and two rows holding NULL are both permitted by the
/// constraint — so the one thing an identity has to promise, that it names one
/// row, is exactly what this cannot.
#[tokio::test]
async fn a_nullable_unique_key_is_refused_by_name() {
    let why = refused(
        r#"{"schema":"public","relation":"invitations","updates":[
             {"key":[{"column":"email","value":"a@example.com"}],
              "set":[{"column":"note","value":"x"}]}]}"#,
    )
    .await;
    assert!(why.contains("invitations_email_key"), "{why}");
    assert!(why.contains("can be null"), "{why}");
}

/// Several candidates, and the same one every time.
///
/// Fewest columns first and then the name: `memberships_alias_key` wins over the
/// two-column key on width and over `memberships_code_key` on name, and it is
/// neither the first nor the last thing the driver reported — so a rule that
/// took whatever arrived first would fail here.
#[tokio::test]
async fn one_of_several_unique_keys_is_chosen_and_it_is_always_the_same_one() {
    let json = r#"{"schema":"public","relation":"memberships","deletes":[
             {"key":[{"column":"alias","value":"a"}]}]}"#;
    for _ in 0..5 {
        assert_eq!(
            written(json).await,
            ["DELETE FROM public.memberships WHERE alias = 'a'"]
        );
    }
}

/// A key naming a column the relation does not have is refused rather than
/// trusted: the two answers contradict each other, and nothing here can say
/// which one is right.
#[tokio::test]
async fn a_unique_key_over_a_column_that_is_not_there_is_refused() {
    let why = refused(
        r#"{"schema":"public","relation":"ghost","deletes":[
             {"key":[{"column":"label","value":"a"}]}]}"#,
    )
    .await;
    assert!(why.contains("ghost_missing_key"), "{why}");
    assert!(why.contains("no column of"), "{why}");
}

/// The exported answer is the one the statements are built from, so a front end
/// that asks does not have to work the rule out again.
#[tokio::test]
async fn the_identity_a_front_end_asks_for_is_the_one_the_where_clause_uses() {
    let named = dbedit::identity(&Fake, "public", "memberships")
        .await
        .unwrap();
    assert_eq!(named.columns, ["alias"]);
    assert_eq!(named.obstacle, None);

    let compound = dbedit::identity(&Fake, "public", "lines").await.unwrap();
    assert_eq!(compound.columns, ["order_id", "line_no"]);

    let refused = dbedit::identity(&Fake, "public", "invitations")
        .await
        .unwrap();
    assert!(refused.columns.is_empty());
    assert!(
        refused
            .obstacle
            .as_deref()
            .is_some_and(|why| why.contains("invitations_email_key")),
        "{refused:?}"
    );
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

/// A NULL cell is asked about with `IS NULL` rather than with `= NULL`.
///
/// The failure this guards against runs: `= NULL` is valid SQL and is never
/// true, so *Filter to this value* over an empty cell would appear to work and
/// return an empty grid — which reads as "this table has no such rows" rather
/// than as a mistake made here.
#[tokio::test]
async fn a_null_cell_is_asked_about_with_is_null() {
    assert_eq!(
        filtered(r#"{"schema":"public","relation":"lines","column":"note","op":"equals"}"#).await,
        "note IS NULL"
    );
    assert_eq!(
        filtered(r#"{"schema":"public","relation":"lines","column":"note","op":"not_equals"}"#)
            .await,
        "note IS NOT NULL"
    );
}

/// A filter writes the value as its column's type reads it and the name as the
/// dialect quotes it — the two facts the front end does not have.
#[tokio::test]
async fn a_cell_filter_is_written_the_way_its_column_reads_it() {
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"qty","op":"equals","value":"2"}"#
        )
        .await,
        "qty = 2"
    );
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"note","op":"not_equals",
                "value":"it's fine"}"#
        )
        .await,
        "note <> 'it''s fine'"
    );
    // The same refusal an edit makes: a typing mistake must not become a
    // predicate that runs against a numeric column.
    let filter: dbedit::CellFilter = serde_json::from_str(
        r#"{"schema":"public","relation":"lines","column":"qty","op":"equals","value":"abc"}"#,
    )
    .unwrap();
    let why = dbedit::cell_filter(&Fake, &dbsql::POSTGRES, &filter)
        .await
        .expect_err("text that is not a number is not a filter")
        .to_string();
    assert!(why.contains("not a number"), "{why}");
}

/// Each ordering operator becomes the comparison it names, with its value
/// written the way its column reads it — bare for a number, quoted for text.
#[tokio::test]
async fn an_ordering_operator_becomes_the_comparison_it_names() {
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"qty","op":"greater_than",
                "value":"2"}"#
        )
        .await,
        "qty > 2"
    );
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"qty","op":"less_or_equal",
                "value":"2"}"#
        )
        .await,
        "qty <= 2"
    );
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"shipped_at",
                "op":"greater_or_equal","value":"2024-01-01"}"#
        )
        .await,
        "shipped_at >= '2024-01-01'"
    );
}

/// The three `LIKE` operators put the wildcards where the operator says, and
/// nowhere the value says.
///
/// The second case is the whole reason `escape_like` exists: a value holding a
/// per cent is ordinary data — a discount, a completion — and read as a wildcard
/// it returns rows nobody asked for while looking exactly like a filter that
/// worked.
#[tokio::test]
async fn a_like_filter_matches_the_characters_that_were_typed() {
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"sku","op":"contains",
                "value":"ab"}"#
        )
        .await,
        r"sku LIKE '%ab%' ESCAPE '\'"
    );
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"sku","op":"starts_with",
                "value":"50%"}"#
        )
        .await,
        r"sku LIKE '50\%%' ESCAPE '\'"
    );
    assert_eq!(
        filtered(
            r#"{"schema":"public","relation":"lines","column":"sku","op":"ends_with",
                "value":"a_b"}"#
        )
        .await,
        r"sku LIKE '%a\_b' ESCAPE '\'"
    );
}

/// A database with no `ESCAPE` clause is refused by name rather than handed a
/// pattern whose wildcards it will read.
#[tokio::test]
async fn a_dialect_without_an_escape_clause_gets_no_like_filter() {
    let filter: dbedit::CellFilter = serde_json::from_str(
        r#"{"schema":"public","relation":"lines","column":"sku","op":"contains","value":"a"}"#,
    )
    .unwrap();
    let why = dbedit::cell_filter(&Fake, &dbsql::CLICKHOUSE, &filter)
        .await
        .expect_err("a LIKE this build cannot escape is not a filter")
        .to_string();
    assert!(why.contains("ESCAPE"), "{why}");
}

/// An operator added since the cell menu, with nothing to compare against, is a
/// row still being typed and is refused.
///
/// `equals` keeps its own answer, which the case above this file pins: over a
/// NULL cell it is `IS NULL`. Both readings are right and they are right about
/// different things, which is why one function has to give two answers.
#[tokio::test]
async fn an_operator_with_no_value_is_an_unfinished_row_and_not_a_question_about_null() {
    let filter: dbedit::CellFilter = serde_json::from_str(
        r#"{"schema":"public","relation":"lines","column":"qty","op":"less_than"}"#,
    )
    .unwrap();
    let why = dbedit::cell_filter(&Fake, &dbsql::POSTGRES, &filter)
        .await
        .expect_err("a comparison against nothing is not a filter")
        .to_string();
    assert!(why.contains("no value to compare against"), "{why}");
}

/// One clause, from the JSON the front end sends.
async fn filtered(json: &str) -> String {
    let filter: dbedit::CellFilter = serde_json::from_str(json).expect("the filter should parse");
    dbedit::cell_filter(&Fake, &dbsql::POSTGRES, &filter)
        .await
        .expect("the clause should be writable")
}
