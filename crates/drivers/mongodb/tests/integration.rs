//! The MongoDB driver against a live server.
//!
//! Marked `ignore`, so `cargo test` passes with nothing installed. To run them:
//!
//! ```text
//! docker run -d --name mongo-test -p 57017:27017 mongo:7
//! cargo test -p driver-mongodb -- --ignored
//! docker rm -f mongo-test
//! ```
//!
//! The fixture is built with the `mongodb` crate directly rather than through
//! the driver, so a fixture never depends on the code under test being right.

use bson::{Bson, Document, doc};
use driver_mongodb::MongoSource;
use mongodb::Client;

const URI: &str = "mongodb://127.0.0.1:57017";

/// Seeds a fixture and returns the driver and the database it is in.
///
/// A database per test, named after it. `cargo test` runs these concurrently and
/// they all begin by dropping what they are about to build, so a single shared
/// name means one test deleting another's collections halfway through — which is
/// exactly what happened, and it looked like a duplicate-key bug in the seed.
///
/// Dropped and rebuilt each time rather than reused. A test that passes only
/// against a database left over from the previous run fails on a clean machine,
/// which is the machine that matters.
async fn fixture(test: &str) -> (MongoSource, String) {
    // Truncated: MongoDB refuses a database name over 63 characters, and these
    // test names are sentences.
    let name = format!("db_{}", &test[..test.len().min(50)]);
    let client = Client::with_uri_str(URI)
        .await
        .expect("MongoDB unreachable; see the header of this file");
    let db = client.database(&name);
    db.drop().await.expect("could not clear the fixture");

    // Uniform: every document has the same two fields, which is the case that
    // must not acquire an overflow column.
    let nums: Vec<Document> = (1..=500)
        .map(|i| doc! { "_id": i, "label": format!("row-{i}") })
        .collect();
    db.collection::<Document>("nums")
        .insert_many(nums)
        .await
        .expect("seeding nums");

    // Ragged, and deliberately longer than the driver's 1000-document sample.
    // `surprise` sits past the end of it, so no schema inferred from a prefix
    // can have a column for that field -- which is the only way to exercise the
    // overflow. An earlier version of this seeded fifty documents, so the sample
    // saw every field and the test proved nothing.
    let mut ragged: Vec<Document> = Vec::new();
    ragged.push(doc! { "_id": 1, "a": 1 });
    ragged.push(doc! { "_id": 2, "a": 2, "b": "two" });
    for i in 3..=1200 {
        ragged.push(doc! { "_id": i, "a": i });
    }
    ragged.push(doc! { "_id": 2000, "a": 1, "surprise": "late" });
    db.collection::<Document>("ragged")
        .insert_many(ragged)
        .await
        .expect("seeding ragged");

    // Types: one document holding each of the mappings that had to be decided.
    db.collection::<Document>("kinds")
        .insert_one(doc! {
            "when": bson::DateTime::from_millis(1_700_000_000_000),
            "flag": true,
            "small": 7i32,
            "big": 5_000_000_000i64,
            "real": 1.5f64,
            "text": "hello",
            "nested": doc! { "city": "Taipei" },
            "list": [1, 2, 3],
            "blob": Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Generic,
                bytes: vec![1, 2, 3],
            }),
        })
        .await
        .expect("seeding kinds");

    db.run_command(doc! {
        "create": "validated",
        "validator": { "$jsonSchema": {
            "bsonType": "object",
            "required": ["email"],
            "properties": { "email": { "bsonType": "string" } },
        }},
        "validationLevel": "strict",
        "validationAction": "error",
    })
    .await
    .expect("creating the validated collection");

    db.run_command(doc! { "create": "recent", "viewOn": "nums", "pipeline": [
        { "$match": { "_id": { "$gt": 400 } } },
    ]})
    .await
    .expect("creating the view");

    db.collection::<Document>("nums")
        .create_index(
            mongodb::IndexModel::builder()
                .keys(doc! { "label": -1 })
                .build(),
        )
        .await
        .expect("creating an index");

    let source = MongoSource::connect(&format!("{URI}/{name}"))
        .await
        .expect("driver could not connect");
    (source, name)
}

fn find(collection: &str) -> String {
    format!(r#"{{"find": "{collection}", "sort": {{"_id": 1}}}}"#)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn reads_a_collection_in_batches_of_the_size_asked_for() {
    let (src, _db) = fixture("reads_a_collection_in_batches_of_the_size_asked_for").await;
    let mut stream = src.query(&find("nums"), 100).await.expect("query");
    assert_eq!(
        stream.schema().fields().len(),
        3,
        "_id, label, and the hatch"
    );

    let mut seen = 0;
    while let Some(batch) = stream.next_batch().await.expect("batch") {
        assert!(batch.num_rows() <= 100);
        seen += batch.num_rows();
    }
    assert_eq!(seen, 500);
    assert_eq!(stream.rows_affected(), Some(500));
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_uniform_collection_gets_its_own_columns_and_the_escape_hatch() {
    // The escape hatch is unconditional, so a uniform collection carries one
    // column that stays empty. That is the price of never losing a value whose
    // type the sample could not have predicted.
    let (src, _db) = fixture("a_uniform_collection_gets_its_own_columns").await;
    let mut stream = src.query(&find("nums"), 50).await.expect("query");
    let schema = stream.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, vec!["_id", "label", "_extra"]);

    while let Some(batch) = stream.next_batch().await.expect("batch") {
        let extra = batch.column_by_name("_extra").expect("present");
        assert!(
            (0..batch.num_rows()).all(|r| extra.is_null(r)),
            "nothing in this collection should overflow"
        );
    }
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_field_no_sample_could_have_seen_is_still_delivered() {
    // The case the whole shape module exists for. `surprise` is on the last
    // document of `ragged`, so a schema settled from the first documents cannot
    // have a column for it -- and it must still reach the caller.
    let (src, _db) = fixture("a_field_no_sample_could_have_seen_is_still_delivered").await;
    let mut stream = src.query(&find("ragged"), 10).await.expect("query");
    assert!(
        stream.schema().field_with_name("_extra").is_ok(),
        "a collection whose documents disagree gets somewhere to put the rest"
    );

    let mut found = false;
    while let Some(batch) = stream.next_batch().await.expect("batch") {
        let column = batch.column_by_name("_extra").expect("the overflow column");
        let extra = arrow::array::cast::as_string_array(column);
        for row in 0..batch.num_rows() {
            if !column.is_null(row) && extra.value(row).contains("surprise") {
                found = true;
            }
        }
    }
    assert!(found, "the late field was dropped instead of carried");
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn each_kind_of_value_arrives_as_the_type_that_was_decided_for_it() {
    use arrow::datatypes::{DataType, TimeUnit};
    let (src, _db) =
        fixture("each_kind_of_value_arrives_as_the_type_that_was_decided_for_it").await;
    let stream = src.query(&find("kinds"), 10).await.expect("query");
    let schema = stream.schema();
    let of = |name: &str| {
        schema
            .field_with_name(name)
            .expect(name)
            .data_type()
            .clone()
    };

    assert_eq!(of("flag"), DataType::Boolean);
    assert_eq!(of("small"), DataType::Int32);
    assert_eq!(of("big"), DataType::Int64);
    assert_eq!(of("real"), DataType::Float64);
    assert_eq!(of("text"), DataType::Utf8);
    assert_eq!(of("blob"), DataType::Binary);
    assert_eq!(
        of("when"),
        DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
    );
    // The deliberate flattening: the front end's Arrow reader handles no nested
    // types, so a document and an array are text rather than Struct and List.
    assert_eq!(of("nested"), DataType::Utf8);
    assert_eq!(of("list"), DataType::Utf8);
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_command_with_no_cursor_is_its_own_one_row_result() {
    let (src, _db) = fixture("a_command_with_no_cursor_is_its_own_one_row_result").await;
    let mut stream = src
        .query(r#"{"count": "nums"}"#, 10)
        .await
        .expect("count is a command, not a query");
    let batch = stream
        .next_batch()
        .await
        .expect("batch")
        .expect("one row of reply");
    assert_eq!(batch.num_rows(), 1);
    assert!(batch.column_by_name("n").is_some(), "the count is in `n`");
}

// ---------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn pages_a_cursor_without_repeating_or_skipping() {
    let (src, _db) = fixture("pages_a_cursor_without_repeating_or_skipping").await;
    let mut cursor = src.cursor(&find("nums"), 50).await.expect("cursor");

    let mut ids: Vec<i32> = Vec::new();
    while let Some(batch) = cursor.fetch().await.expect("fetch") {
        let column = arrow::array::cast::as_primitive_array::<arrow::datatypes::Int32Type>(
            batch.column_by_name("_id").expect("_id"),
        );
        ids.extend((0..batch.num_rows()).map(|r| column.value(r)));
    }
    assert_eq!(ids.len(), 500, "every document once");
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "in order, with nothing repeated"
    );
    cursor.close().await.expect("close");
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn cancelling_an_idle_cursor_is_not_a_failure() {
    // Delivery is not interruption: pressing Cancel when nothing is running has
    // to succeed, or a front end reports a failure for pressing a button at the
    // wrong moment.
    let (src, _db) = fixture("cancelling_an_idle_cursor_is_not_a_failure").await;
    let cursor = src.cursor(&find("nums"), 10).await.expect("cursor");
    cursor.canceller().cancel().await.expect("cancel");
    src.cancel().await.expect("session cancel");
}

// ---------------------------------------------------------------------------
// Failure
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_statement_the_server_rejects_is_not_reported_as_cancelled() {
    let (src, _db) = fixture("a_statement_the_server_rejects_is_not_reported_as_cancelled").await;
    let err = match src
        .query(r#"{"find": "nums", "sort": "sideways"}"#, 10)
        .await
    {
        Err(e) => e,
        Ok(mut s) => match s.next_batch().await {
            Err(e) => e,
            Ok(_) => panic!("a sort that is not a document should be refused"),
        },
    };
    assert!(!err.is_cancelled(), "a rejection is not a cancellation");
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn the_navigator_finds_the_database_and_its_collections() {
    let (src, db) = fixture("the_navigator_finds_the_database_and_its_collections").await;
    let schemas = src.schemas().await.expect("schemas");
    assert!(schemas.iter().any(|s| s.name == *db));

    let relations = src.relations(&db).await.expect("relations");
    let nums = relations
        .iter()
        .find(|r| r.name == "nums")
        .expect("nums should be listed");
    assert_eq!(nums.kind, dbconn::RelationKind::Table);
    assert_eq!(nums.schema, db);

    let view = relations
        .iter()
        .find(|r| r.name == "recent")
        .expect("the view should be listed");
    assert_eq!(view.kind, dbconn::RelationKind::View);
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_collections_fields_are_found_by_looking_at_documents() {
    let (src, db) = fixture("a_collections_fields_are_found_by_looking_at_documents").await;
    let columns = src.columns(&db, "nums").await.expect("columns");
    // Two, not three: the structure pane lists the fields documents actually
    // have, and `_extra` is a column this client adds to results.
    assert_eq!(columns.len(), 2);
    // One-based and ascending, as every other driver reports.
    for (at, column) in columns.iter().enumerate() {
        assert_eq!(column.position, at as i32 + 1);
        assert!(!column.data_type.is_empty());
    }
    let id = &columns[0];
    assert_eq!(id.name, "_id");
    assert!(id.is_primary_key, "_id is the one key MongoDB guarantees");
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_nested_field_reports_the_type_name_the_value_viewer_reads() {
    // The seam between the two halves of one decision. `ColumnType::Document` is
    // a Rust variant; what crosses to the app is the string `metadata::columns`
    // derives from its name, and `ValueRendering.isJSONType` matches that string
    // to decide whether to lay the document out over lines. Nothing carries the
    // name across, so renaming the variant would leave the unit tests on this
    // side and the checks on that one both passing, with every document back on
    // the single line the viewer exists to escape.
    let (src, db) = fixture("a_nested_field_reports_the_type_name_the_viewer_reads").await;
    let columns = src.columns(&db, "kinds").await.expect("columns");
    let named = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} should be a column of kinds"))
            .data_type
            .clone()
    };

    assert_eq!(named("nested"), "document");
    assert_eq!(named("list"), "document", "an array is nested too");
    // And the catch-all this was split out of keeps its own name: a column of
    // ObjectIds must never be handed to a JSON parser.
    assert_eq!(named("text"), "text");
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_view_states_what_it_is_a_view_on() {
    let (src, db) = fixture("a_view_states_what_it_is_a_view_on").await;
    let definition = src
        .definition(&db, "recent")
        .await
        .expect("definition")
        .expect("a view has one");
    assert!(definition.contains("nums"), "got: {definition}");
    assert!(definition.contains("$match"), "got: {definition}");
    // A collection is not a view, which is what the structure pane hangs a
    // section on.
    assert_eq!(src.definition(&db, "nums").await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn indexes_report_their_direction_and_the_key_that_is_always_there() {
    let (src, db) =
        fixture("indexes_report_their_direction_and_the_key_that_is_always_there").await;
    let indexes = src.indexes(&db, "nums").await.expect("indexes");

    let id = indexes
        .iter()
        .find(|i| i.is_primary)
        .expect("_id is always indexed");
    assert!(id.is_unique);

    let label = indexes
        .iter()
        .find(|i| i.name == "label_-1")
        .expect("the seeded index");
    assert_eq!(
        label.columns,
        vec!["label DESC"],
        "a descending key is not the same index as an ascending one"
    );
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_validator_is_reported_as_the_check_constraint_it_is() {
    // The call this driver was expected to leave empty. A collection that
    // rejects writes has a constraint, whatever MongoDB calls it.
    let (src, db) = fixture("a_validator_is_reported_as_the_check_constraint_it_is").await;
    let constraints = src
        .constraints(&db, "validated")
        .await
        .expect("constraints");
    assert_eq!(constraints.len(), 1);
    assert_eq!(constraints[0].kind, dbconn::ConstraintKind::Check);
    assert!(constraints[0].definition.contains("email"));

    assert!(
        src.constraints(&db, "nums").await.unwrap().is_empty(),
        "a collection with no validator has no constraints"
    );
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn the_calls_this_database_has_no_answer_for_are_empty_rather_than_broken() {
    let (src, db) =
        fixture("the_calls_this_database_has_no_answer_for_are_empty_rather_than_broken").await;
    assert!(src.foreign_keys(&db, "nums").await.unwrap().is_empty());
    assert!(src.referenced_by(&db, "nums").await.unwrap().is_empty());
    assert!(src.triggers(&db, "nums").await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn asking_about_a_collection_that_is_not_there_is_an_empty_answer() {
    // A navigator works from a tree that can be one refresh out of date, so this
    // happens in ordinary use and must not put an error on screen.
    let (src, db) = fixture("asking_about_a_collection_that_is_not_there_is_an_empty_answer").await;
    let missing = "no_such_collection_anywhere";
    assert!(src.columns(&db, missing).await.unwrap().is_empty());
    assert!(src.indexes(&db, missing).await.unwrap().is_empty());
    assert!(src.constraints(&db, missing).await.unwrap().is_empty());
    assert!(src.triggers(&db, missing).await.unwrap().is_empty());
    assert_eq!(src.definition(&db, missing).await.unwrap(), None);
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn staged_changes_are_written_run_and_read_back() {
    // The one thing the unit tests in `edits.rs` cannot show: that the documents
    // this driver writes are documents this server accepts, and that an edit to
    // an Int32 field leaves it an Int32 rather than widening it on the way
    // through JSON.
    let (src, db) = fixture("staged_changes_are_written_run_and_read_back").await;
    let client = Client::with_uri_str(URI).await.expect("a second client");
    let people = client.database(&db).collection::<Document>("people");
    let ada = bson::oid::ObjectId::new();
    let grace = bson::oid::ObjectId::new();
    people
        .insert_many(vec![
            doc! { "_id": ada, "name": "Ada", "seats": 2i32 },
            doc! { "_id": grace, "name": "Grace", "seats": 3i32 },
        ])
        .await
        .expect("seeding people");

    let staged: dbconn::RowEdits = serde_json::from_str(&format!(
        r#"{{"schema": "{db}", "relation": "people",
            "updates": [{{"key": [{{"column": "_id", "value": "{ada}"}}],
                          "set": [{{"column": "name", "value": "Ada L"}},
                                  {{"column": "seats", "value": "5"}}]}}],
            "inserts": [{{"set": [{{"column": "name", "value": "Kay"}},
                                  {{"column": "seats", "value": "9"}}]}}],
            "deletes": [{{"key": [{{"column": "_id", "value": "{grace}"}}]}}]}}"#,
        ada = ada.to_hex(),
        grace = grace.to_hex(),
    ))
    .expect("the edits should parse");

    let statements = dbconn::Driver::write_rows(&src, &staged)
        .await
        .expect("the driver should write these");
    assert_eq!(statements.len(), 3, "one each: {statements:?}");
    for statement in &statements {
        let mut stream = src
            .query(statement, 10)
            .await
            .expect("the server accepts it");
        while stream.next_batch().await.expect("reply").is_some() {}
    }

    let changed = people
        .find_one(doc! { "_id": ada })
        .await
        .expect("read back")
        .expect("Ada is still there");
    assert_eq!(changed.get_str("name").expect("name"), "Ada L");
    assert_eq!(
        changed.get("seats"),
        Some(&Bson::Int32(5)),
        "an int32 field is still int32 after an edit"
    );
    assert!(
        people
            .find_one(doc! { "name": "Kay" })
            .await
            .expect("read back")
            .is_some(),
        "the inserted document is there, under an id the server chose"
    );
    assert!(
        people
            .find_one(doc! { "_id": grace })
            .await
            .expect("read back")
            .is_none(),
        "the deleted document is gone"
    );
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn an_id_is_read_as_an_id_and_written_back_as_one() {
    // The round trip the whole of `edits.rs` turns on: the grid shows 24 hex
    // digits, the column says those digits are an ObjectId, and the update
    // written from them reaches the document rather than matching nothing.
    let (src, db) = fixture("an_id_is_read_as_an_id_and_written_back_as_one").await;
    let client = Client::with_uri_str(URI).await.expect("a second client");
    let notes = client.database(&db).collection::<Document>("notes");
    let id = bson::oid::ObjectId::new();
    notes
        .insert_one(doc! { "_id": id, "body": "before" })
        .await
        .expect("seeding notes");

    let key = src.columns(&db, "notes").await.expect("columns");
    assert_eq!(
        key.iter().find(|c| c.name == "_id").expect("_id").data_type,
        "objectid",
        "the column says what the digits are"
    );

    let staged: dbconn::RowEdits = serde_json::from_str(&format!(
        r#"{{"schema": "{db}", "relation": "notes",
            "updates": [{{"key": [{{"column": "_id", "value": "{}"}}],
                          "set": [{{"column": "body", "value": "after"}}]}}],
            "inserts": [], "deletes": []}}"#,
        id.to_hex()
    ))
    .expect("the edits should parse");
    let statements = dbconn::Driver::write_rows(&src, &staged)
        .await
        .expect("written");
    let mut stream = src.query(&statements[0], 10).await.expect("accepted");
    while stream.next_batch().await.expect("reply").is_some() {}

    let after = notes
        .find_one(doc! { "_id": id })
        .await
        .expect("read back")
        .expect("still there");
    assert_eq!(after.get_str("body").expect("body"), "after");
}

#[tokio::test]
#[ignore = "requires a MongoDB server"]
async fn a_change_this_database_cannot_express_is_refused_before_anything_is_sent() {
    let (src, db) =
        fixture("a_change_this_database_cannot_express_is_refused_before_anything_is_sent").await;
    let staged: dbconn::RowEdits = serde_json::from_str(&format!(
        r#"{{"schema": "{db}", "relation": "kinds",
            "updates": [{{"key": [{{"column": "text", "value": "hello"}}],
                          "set": [{{"column": "blob", "value": "<3 bytes>"}}]}}],
            "inserts": [], "deletes": []}}"#
    ))
    .expect("the edits should parse");
    let why = dbconn::Driver::write_rows(&src, &staged)
        .await
        .expect_err("a binary field cannot be written back")
        .to_string();
    assert!(why.contains("how many bytes"), "{why}");
}
