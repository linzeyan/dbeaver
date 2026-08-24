//! What MongoDB can say about itself, in the shape the navigator expects.
//!
//! Four of the nine calls return nothing and issue no command to find out, which
//! is a statement about the database rather than a gap in this file: MongoDB has
//! no foreign keys and no triggers, so `foreign_keys`, `referenced_by` and
//! `triggers` have nothing to answer with. Returning empty is the honest answer
//! and asking the server first would only be slower.
//!
//! `constraints` is the one that turned out not to be empty. A collection can
//! carry a JSON Schema validator, which rejects writes exactly as a `CHECK`
//! does, and a structure pane that said "no constraints" beside a collection
//! refusing every insert would be actively misleading.

use bson::{Bson, Document, doc};
use dbconn::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo, UniqueKeyInfo,
};
use mongodb::Cursor as MongoCursor;

use crate::{MongoError, MongoSource, SAMPLE, Shape};

/// How many documents `columns` reads to work out what the fields are.
///
/// Smaller than the sample a result uses, because this one runs every time
/// somebody clicks a collection in the sidebar and a navigator that stalls is a
/// navigator people stop opening. The cost of the smaller number is a rare field
/// missing from the structure pane, which is recoverable by looking at the data;
/// the cost of the larger one is paid on every click.
const PEEK: usize = 200;

impl MongoSource {
    /// The databases on this deployment.
    ///
    /// MongoDB's namespace is deployment → database → collection, which lines up
    /// with the trait's server → schema → relation exactly. This is the only
    /// database in the phase-2 set that needed no level flattened away or
    /// invented.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, MongoError> {
        // Three of them belong to the deployment rather than to anybody's
        // application: `admin` holds the users and roles, `config` is the
        // sharding metadata, and `local` is the replication oplog. They were
        // listed beside the data until the tree learned to tell them apart, and
        // on a fresh deployment they were most of what it listed.
        let names = self.client().list_database_names().await?;
        Ok(names
            .into_iter()
            .map(|name| SchemaInfo {
                is_system: matches!(name.as_str(), "admin" | "config" | "local"),
                name,
            })
            .collect())
    }

    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, MongoError> {
        let db = self.database(schema);
        let mut cursor: MongoCursor<Document> = db
            .run_cursor_command(doc! { "listCollections": 1, "cursor": {} })
            .await?;

        let mut out = Vec::new();
        while cursor.advance().await? {
            let entry = cursor.deserialize_current()?;
            let name = entry.get_str("name").unwrap_or_default().to_string();
            if name.is_empty() {
                continue;
            }
            let kind = match entry.get_str("type").unwrap_or("collection") {
                "view" => RelationKind::View,
                // A timeseries collection is a view over a hidden bucket
                // collection, but it is created, written and read as a
                // collection, so listing it as a view would put it under the
                // wrong heading and offer the wrong actions.
                "timeseries" => RelationKind::Table,
                "collection" => RelationKind::Table,
                _ => RelationKind::Unknown,
            };
            out.push(RelationInfo {
                schema: schema.to_string(),
                name,
                kind,
                // Deliberately not filled. `count` on every collection in the
                // list is a command each, and the fast `estimatedDocumentCount`
                // reads collection metadata that is documented as approximate
                // and is simply wrong after an unclean shutdown. `None` means
                // "nothing has measured this", which is exactly the case.
                estimated_rows: None,
            });
        }
        Ok(out)
    }

    /// The fields of a collection, inferred from documents in it.
    ///
    /// The call with no honest answer. Every other database is asked what its
    /// columns are; here they have to be found out, and what comes back
    /// describes the documents that were looked at rather than the collection.
    /// A field used by one document in ten thousand will not appear.
    ///
    /// `nullable` is therefore always true, and it means something weaker than
    /// elsewhere: not "the schema permits null" — there is no schema — but "this
    /// field was missing from at least one document, or could be". Claiming
    /// otherwise from a sample would be inferring a guarantee from an
    /// observation.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, MongoError> {
        let documents = self.peek(schema, relation, PEEK).await?;
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let shape = Shape::infer(&documents);
        Ok(shape
            .columns()
            .into_iter()
            .enumerate()
            .map(|(at, (name, ty))| ColumnInfo {
                // `_id` is the one field MongoDB does guarantee: it is present
                // in every document, unique, and indexed. That is a primary key
                // by every property that matters.
                is_primary_key: name == "_id",
                data_type: format!("{ty:?}").to_lowercase(),
                name,
                nullable: true,
                position: at as i32 + 1,
                default_value: None,
                computed: None,
            })
            .collect())
    }

    /// A view's pipeline, as the aggregation it is.
    ///
    /// Not SQL, and not pretending to be. A MongoDB view is `viewOn` plus a
    /// pipeline, and the pipeline printed as JSON is the definition in the form
    /// the user would have written it.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, MongoError> {
        let Some(entry) = self.collection_entry(schema, relation).await? else {
            return Ok(None);
        };
        if entry.get_str("type").unwrap_or("collection") != "view" {
            return Ok(None);
        }
        let options = entry.get_document("options").ok();
        let Some(options) = options else {
            return Ok(None);
        };
        let on = options.get_str("viewOn").unwrap_or_default();
        let pipeline = options.get_array("pipeline").ok();
        let rendered = match pipeline {
            Some(stages) => {
                serde_json::to_string_pretty(&Bson::Array(stages.clone()).into_relaxed_extjson())
                    .unwrap_or_default()
            }
            None => "[]".to_string(),
        };
        Ok(Some(format!("on {on}\n{rendered}")))
    }

    /// Empty, always, and without asking — although MongoDB has unique indexes
    /// and `indexes` reports them.
    ///
    /// What it cannot report is the other half of the rule. A unique key names
    /// one row only if its fields cannot be null, and a collection does not
    /// declare its fields: `columns` samples documents and infers them, so
    /// "nullable" here is a description of the documents that happened to be
    /// read rather than a promise about the ones that have not been. A unique
    /// index in MongoDB also allows one document missing the field entirely, so
    /// two of them can differ in nothing this could put in a filter.
    ///
    /// Returning the indexes anyway would move that guess into the one place the
    /// rule is decided, which is where it would stop being visible.
    pub async fn unique_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, MongoError> {
        Ok(Vec::new())
    }

    pub async fn indexes(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<IndexInfo>, MongoError> {
        let db = self.database(schema);
        let mut cursor: MongoCursor<Document> = match db
            .run_cursor_command(doc! { "listIndexes": relation, "cursor": {} })
            .await
        {
            Ok(c) => c,
            // 26 is NamespaceNotFound. A collection that is not there has no
            // indexes, which is an empty answer and not a failure -- the
            // navigator works from a tree that can be one refresh out of date.
            Err(e) if crate::code_of(&e) == Some(26) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut out = Vec::new();
        while cursor.advance().await? {
            let entry = cursor.deserialize_current()?;
            let name = entry.get_str("name").unwrap_or_default().to_string();
            let key = entry.get_document("key").cloned().unwrap_or_default();
            out.push(IndexInfo {
                is_primary: name == "_id_",
                name: name.clone(),
                is_unique: entry.get_bool("unique").unwrap_or(false) || name == "_id_",
                // The direction is part of the key and not decoration: a
                // compound index on `{a: 1, b: -1}` serves a different sort from
                // one on `{a: 1, b: 1}`, and printing both as "a, b" would claim
                // the planner can use an index it cannot.
                method: method_of(&key),
                columns: key
                    .iter()
                    .map(|(field, direction)| match direction {
                        Bson::Int32(-1) => format!("{field} DESC"),
                        Bson::Int32(1) | Bson::Double(_) => field.clone(),
                        other => format!("{field} {}", describe(other)),
                    })
                    .collect(),
                predicate: entry.get_document("partialFilterExpression").ok().map(|f| {
                    serde_json::to_string(&Bson::Document(f.clone()).into_relaxed_extjson())
                        .unwrap_or_default()
                }),
            });
        }
        Ok(out)
    }

    /// Empty, always, and without asking. MongoDB declares no foreign keys —
    /// references between collections exist in application code and nowhere the
    /// server can see them.
    pub async fn foreign_keys(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MongoError> {
        Ok(Vec::new())
    }

    /// Empty for the same reason as `foreign_keys`: there is nothing declared to
    /// look up from the other end either.
    pub async fn referenced_by(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MongoError> {
        Ok(Vec::new())
    }

    /// A collection's validator, which is a check constraint in all but name.
    ///
    /// The call this driver was expected to leave empty and does not. MongoDB
    /// lets a collection carry a JSON Schema — or any query expression — that
    /// every write must satisfy, and `validationLevel` and `validationAction`
    /// say how strictly. A structure pane reporting no constraints beside a
    /// collection that is rejecting the user's inserts would be worse than
    /// showing nothing at all.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, MongoError> {
        let Some(entry) = self.collection_entry(schema, relation).await? else {
            return Ok(Vec::new());
        };
        let Ok(options) = entry.get_document("options") else {
            return Ok(Vec::new());
        };
        let Ok(validator) = options.get_document("validator") else {
            return Ok(Vec::new());
        };
        if validator.is_empty() {
            return Ok(Vec::new());
        }

        let level = options.get_str("validationLevel").unwrap_or("strict");
        let action = options.get_str("validationAction").unwrap_or("error");
        let body =
            serde_json::to_string_pretty(&Bson::Document(validator.clone()).into_relaxed_extjson())
                .unwrap_or_default();
        Ok(vec![ConstraintInfo {
            // MongoDB does not name validators, so there is one per collection
            // and it is named for what it is. A SQLite foreign key had the same
            // problem and its driver builds a name the same way.
            name: format!("{relation}_validator"),
            kind: ConstraintKind::Check,
            definition: format!("level {level}, on failure {action}\n{body}"),
        }])
    }

    /// Empty, always. MongoDB has no triggers.
    ///
    /// Change streams are the thing people reach for instead, and they are not
    /// triggers: they are a feed a client subscribes to, running in the client,
    /// with no record of them on the server. There is nothing here to list.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, MongoError> {
        Ok(Vec::new())
    }

    // ---- helpers --------------------------------------------------------

    /// The first `limit` documents of a collection, or nothing if it is not
    /// there.
    ///
    /// The first rather than a random `$sample`, and for the same reason the
    /// result sampler takes a prefix: `$sample` returns different documents each
    /// time, so the structure pane would list different fields in a different
    /// order on every refresh of a collection nobody had changed.
    async fn peek(
        &self,
        schema: &str,
        relation: &str,
        limit: usize,
    ) -> Result<Vec<Document>, MongoError> {
        let db = self.database(schema);
        let mut cursor: MongoCursor<Document> = match db
            .run_cursor_command(doc! { "find": relation, "limit": limit as i64 })
            .await
        {
            Ok(c) => c,
            Err(e) if crate::code_of(&e) == Some(26) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::with_capacity(limit.min(SAMPLE));
        while cursor.advance().await? {
            out.push(cursor.deserialize_current()?);
        }
        Ok(out)
    }

    /// One collection's entry in `listCollections`, which carries its type and
    /// its options.
    async fn collection_entry(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<Document>, MongoError> {
        let db = self.database(schema);
        let mut cursor: MongoCursor<Document> = db
            .run_cursor_command(doc! {
                "listCollections": 1,
                "filter": { "name": relation },
                "cursor": {},
            })
            .await?;
        if cursor.advance().await? {
            return Ok(Some(cursor.deserialize_current()?));
        }
        Ok(None)
    }
}

/// The access method an index key describes.
///
/// MongoDB does not report one, so it is read off the key: a value of 1 or -1 is
/// an ordinary B-tree, and anything else is the name of the kind of index it is.
/// `btree` is the right default for the same reason the trait says so — a
/// database with one method reports that one.
fn method_of(key: &Document) -> String {
    for (_, direction) in key {
        if let Bson::String(kind) = direction {
            return kind.clone();
        }
    }
    "btree".to_string()
}

fn describe(value: &Bson) -> String {
    match value {
        Bson::String(s) => s.clone(),
        other => other.to_string(),
    }
}
