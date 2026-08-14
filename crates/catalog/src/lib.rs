//! The names a database holds, cached, and what to offer at a caret.
//!
//! `dbsql` works out what a name at the caret would *mean* — a column of these
//! three relations, beginning with `cust` — and stops there, because it does
//! not talk to databases. This is the other half: it asks the driver what is
//! actually there, remembers the answer, and turns the question into a list.
//!
//! Remembering is the whole of the performance story. Completion runs on a
//! keystroke and the catalog is on the far side of a socket, so a call per
//! keystroke would be a client that pauses while you type. Each answer is
//! fetched once and kept: the schemas of a connection, the relations of a
//! schema, the columns of a relation. Filtering a few thousand remembered names
//! by prefix is free by comparison, which is what makes the third exit
//! criterion of this phase a caching question rather than a search one.
//!
//! Nothing here is invalidated on a timer. A navigator that quietly refetched
//! would make a schema change appear at a moment nobody chose; `forget` is
//! called by the refresh the user asks for.

use dbconn::{ColumnInfo, DbResult, Driver, RelationInfo};
use dbsql::{Completion, Dialect, Expect};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// One thing that could be typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// What to show in the list: the name as the catalog holds it.
    pub label: String,
    /// What to put in the buffer, quoted if this database needs it to be.
    pub insert: String,
    pub kind: Kind,
    /// The second line: a column's type, a relation's schema and kind.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Schema,
    Relation,
    Column,
    /// A CTE or a derived table — a name this statement invented, which the
    /// catalog has never heard of.
    Local,
}

/// What a connection holds, as far as anybody has asked.
pub struct Names {
    driver: Arc<dyn Driver>,
    dialect: &'static Dialect,
    schemas: RwLock<Option<Vec<String>>>,
    relations: RwLock<HashMap<String, Vec<RelationInfo>>>,
    columns: RwLock<HashMap<(String, String), Vec<ColumnInfo>>>,
    /// Where an unqualified relation is looked for.
    default_schema: RwLock<Option<String>>,
}

impl Names {
    pub fn new(driver: Arc<dyn Driver>, dialect: &'static Dialect) -> Self {
        Self {
            driver,
            dialect,
            schemas: RwLock::new(None),
            relations: RwLock::new(HashMap::new()),
            columns: RwLock::new(HashMap::new()),
            default_schema: RwLock::new(None),
        }
    }

    /// Drops everything remembered, so the next question asks the server.
    ///
    /// For the refresh a user presses, and for after a statement that changed
    /// the schema. Not on a timer: a name appearing or vanishing at a moment
    /// nobody chose is worse than one that is a few minutes stale.
    pub async fn forget(&self) {
        *self.schemas.write().await = None;
        self.relations.write().await.clear();
        self.columns.write().await.clear();
    }

    /// The schema an unqualified name is looked for in.
    ///
    /// The first one the driver lists, which is the driver's own ordering and
    /// not a guess made here — `schemas()` answers with the navigator root, and
    /// for the databases with no schema layer of their own that is the one
    /// container they have.
    pub async fn default_schema(&self) -> DbResult<Option<String>> {
        if let Some(name) = self.default_schema.read().await.clone() {
            return Ok(Some(name));
        }
        let first = self.schemas().await?.first().cloned();
        if let Some(name) = &first {
            *self.default_schema.write().await = Some(name.clone());
        }
        Ok(first)
    }

    /// Overrides which schema an unqualified name is looked for in, for a front
    /// end that lets the user choose.
    pub async fn set_default_schema(&self, schema: Option<String>) {
        *self.default_schema.write().await = schema;
    }

    pub async fn schemas(&self) -> DbResult<Vec<String>> {
        if let Some(known) = self.schemas.read().await.clone() {
            return Ok(known);
        }
        let fetched: Vec<String> = self
            .driver
            .schemas()
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect();
        *self.schemas.write().await = Some(fetched.clone());
        Ok(fetched)
    }

    pub async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>> {
        if let Some(known) = self.relations.read().await.get(schema) {
            return Ok(known.clone());
        }
        let fetched = self.driver.relations(schema).await?;
        self.relations
            .write()
            .await
            .insert(schema.to_string(), fetched.clone());
        Ok(fetched)
    }

    pub async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>> {
        let key = (schema.to_string(), relation.to_string());
        if let Some(known) = self.columns.read().await.get(&key) {
            return Ok(known.clone());
        }
        let fetched = self.driver.columns(schema, relation).await?;
        self.columns.write().await.insert(key, fetched.clone());
        Ok(fetched)
    }

    /// What to offer for `question`, best first.
    ///
    /// A failure to read the catalog is not a failure to complete: the names
    /// that were already known are still offered, because an editor that
    /// stopped suggesting anything because one metadata call timed out is worse
    /// than one that suggests slightly less.
    pub async fn suggest(&self, question: &Completion) -> Vec<Suggestion> {
        let mut out = match &question.expect {
            Expect::Nothing => Vec::new(),
            Expect::Statement => self.verbs(),
            Expect::Relation { schema } => self.relation_names(schema.as_deref()).await,
            Expect::Column { qualifier } => self.column_names(question, qualifier.as_deref()).await,
        };
        rank(&mut out, &question.prefix);
        out
    }

    /// The words a statement can begin with.
    fn verbs(&self) -> Vec<Suggestion> {
        VERBS
            .iter()
            .map(|word| Suggestion {
                label: word.to_string(),
                insert: word.to_string(),
                kind: Kind::Keyword,
                detail: String::new(),
            })
            .collect()
    }

    async fn relation_names(&self, schema: Option<&str>) -> Vec<Suggestion> {
        let mut out = Vec::new();
        match schema {
            // `sales.` — the schema is settled, so only what is in it.
            Some(schema) => out.extend(self.relations_in(schema).await),
            None => {
                // Unqualified: the relations of the default schema, and the
                // other schemas by name so that a table elsewhere is two
                // keystrokes rather than unreachable.
                if let Ok(Some(default)) = self.default_schema().await {
                    out.extend(self.relations_in(&default).await);
                }
                if let Ok(schemas) = self.schemas().await {
                    let default = self.default_schema().await.ok().flatten();
                    out.extend(
                        schemas
                            .into_iter()
                            .filter(|s| Some(s) != default.as_ref())
                            .map(|name| Suggestion {
                                insert: self.dialect.quote(&name),
                                label: name,
                                kind: Kind::Schema,
                                detail: "schema".to_string(),
                            }),
                    );
                }
            }
        }
        out
    }

    async fn relations_in(&self, schema: &str) -> Vec<Suggestion> {
        self.relations(schema)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| Suggestion {
                insert: self.dialect.quote(&r.name),
                label: r.name,
                kind: Kind::Relation,
                detail: format!("{:?} in {}", r.kind, r.schema).to_lowercase(),
            })
            .collect()
    }

    async fn column_names(
        &self,
        question: &Completion,
        qualifier: Option<&str>,
    ) -> Vec<Suggestion> {
        // Qualified by a name: only that relation's columns, and nothing at all
        // when the name resolves to nothing. Offering every column in the
        // database after `x.` where `x` is a typo is worse than an empty list,
        // which at least says the qualifier is wrong.
        let wanted: Vec<_> = match qualifier {
            Some(q) => question
                .sources
                .iter()
                .filter(|s| s.handle().eq_ignore_ascii_case(q))
                .collect(),
            None => question.sources.iter().collect(),
        };

        let mut out = Vec::new();
        for source in wanted {
            if source.derived {
                // A CTE or a derived table. Its columns are whatever its own
                // SELECT list produced, which is a question for a later phase;
                // offering the name itself is honest and useful.
                out.push(Suggestion {
                    insert: self.dialect.quote(source.handle()),
                    label: source.handle().to_string(),
                    kind: Kind::Local,
                    detail: "defined in this statement".to_string(),
                });
                continue;
            }
            let schema = match &source.schema {
                Some(s) => s.clone(),
                None => match self.default_schema().await {
                    Ok(Some(s)) => s,
                    _ => continue,
                },
            };
            for column in self
                .columns(&schema, &source.name)
                .await
                .unwrap_or_default()
            {
                out.push(Suggestion {
                    insert: self.dialect.quote(&column.name),
                    label: column.name,
                    kind: Kind::Column,
                    detail: format!("{} · {}", column.data_type, source.handle()),
                });
            }
        }
        out
    }
}

/// The words a statement can begin with, which is a much shorter list than the
/// keywords a dialect has.
///
/// Not read from the dialect table: that table answers "is this word painted",
/// and three hundred words offered at the start of an empty line is not a
/// suggestion list, it is a dictionary.
const VERBS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "CREATE", "ALTER", "DROP", "TRUNCATE",
    "EXPLAIN", "GRANT", "REVOKE", "BEGIN", "COMMIT", "ROLLBACK", "SET", "SHOW",
];

/// Orders suggestions by how well they answer what has been typed, and drops
/// the ones that do not.
///
/// Prefix matches first, then names that merely contain the text. The second
/// group is worth having — somebody looking for `customer_orders` may type
/// `orders` — and worth keeping below the first, because when a prefix matches
/// it is almost always what was meant.
fn rank(out: &mut Vec<Suggestion>, prefix: &str) {
    if prefix.is_empty() {
        out.sort_by(|a, b| {
            a.kind_order()
                .cmp(&b.kind_order())
                .then(a.label.cmp(&b.label))
        });
        return;
    }
    let needle = prefix.to_lowercase();
    out.retain(|s| s.label.to_lowercase().contains(&needle));
    out.sort_by_key(|s| {
        let name = s.label.to_lowercase();
        let starts = if name.starts_with(&needle) { 0 } else { 1 };
        (starts, s.kind_order(), name)
    });
}

impl Suggestion {
    /// Columns before relations before schemas before keywords, when nothing
    /// else separates them. A caret in a statement is far more often reaching
    /// for a column than for a verb.
    fn kind_order(&self) -> u8 {
        match self.kind {
            Kind::Column => 0,
            Kind::Local => 1,
            Kind::Relation => 2,
            Kind::Schema => 3,
            Kind::Keyword => 4,
        }
    }
}
