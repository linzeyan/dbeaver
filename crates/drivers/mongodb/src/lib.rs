//! MongoDB, behind the same `Driver` trait as the SQL databases.
//!
//! This driver is in phase 2 to find out whether the abstraction derived from
//! PostgreSQL and SQLite describes *databases* or merely describes SQL. Most of
//! it survived. The three places it did not are worth stating up front, because
//! they are the finding:
//!
//! **The statement is not SQL, and the trait never said it was.** `query` takes
//! a `&str` and MongoDB's is an MQL command document written as JSON —
//! `{"find": "orders", "filter": {"status": "open"}}`. The parameter is *named*
//! `sql`, which is now the wrong word, but nothing about the trait's shape had
//! to change: a statement is text the database understands, and this database
//! understands JSON. The name is recorded as a wart rather than fixed here,
//! because renaming it touches every driver and that is a decision for after all
//! six report.
//!
//! **The columns are not known before the rows.** Every other database describes
//! its result first. MongoDB cannot, because a collection has no schema and two
//! documents in it may share no field at all. `ResultStream::schema()` still
//! returns before the first batch — it is inferred from a fixed prefix of the
//! result, and `shape.rs` is entirely about what happens to a document that then
//! does not fit. Nothing is dropped.
//!
//! **Four of the nine metadata calls are structurally empty.** MongoDB has no
//! foreign keys and no triggers, so `foreign_keys`, `referenced_by` and
//! `triggers` answer with nothing and issue no query to find out. `constraints`
//! is not empty, which was a surprise: a collection's JSON Schema validator is a
//! check constraint in everything but name, and reporting it is the difference
//! between a structure pane that says "no constraints" and one that shows the
//! rule rejecting the user's insert.

mod driver;
mod metadata;
mod shape;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use bson::{Bson, Document, doc};
use mongodb::{Client, Cursor as MongoCursor, Database};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub use shape::Shape;

/// How many documents are read before the columns are settled.
///
/// Fixed, and deliberately not `batch_rows`. If the sample were the page size
/// then changing how much the grid fetches at a time would change which columns
/// it has, and two users looking at the same collection would see different
/// tables. It is also the first server batch in practice, so it costs no extra
/// round trip.
const SAMPLE: usize = 1000;

/// The verbs whose reply is a cursor rather than a single document.
///
/// A closed list because the two kinds of command are read completely
/// differently and the reply does not announce which it is until it has already
/// been asked for the wrong way. Everything not listed here is run as a plain
/// command and its reply shown as one row, which is the right answer for
/// `count`, `distinct`, `insert`, `update`, `delete` and `explain` alike.
const CURSOR_COMMANDS: &[&str] = &[
    "find",
    "aggregate",
    "listCollections",
    "listIndexes",
    "listSearchIndexes",
];

#[derive(Debug, thiserror::Error)]
pub enum MongoError {
    #[error("{0}")]
    Mongo(#[from] mongodb::error::Error),
    /// The statement was not a JSON object. Carries the offset so a caret can
    /// land on it — see `statement_position`.
    #[error("{message}")]
    Statement {
        message: String,
        position: Option<u32>,
    },
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
}

impl MongoError {
    /// Where in the statement the trouble is: 1-based, counted in characters.
    ///
    /// Only a malformed command document has one, and it is a real position
    /// rather than a courtesy — `serde_json` reports the line and column it gave
    /// up at, and `offset_of` turns that into the offset the trait asks for. A
    /// command that parsed and was then rejected by the server has none:
    /// MongoDB reports which field it disliked by name, not by where the field
    /// was written, and manufacturing an offset from a field name would put the
    /// caret on the first place that name happened to appear.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            MongoError::Statement { position, .. } => *position,
            _ => None,
        }
    }

    /// Whether this is the cancel button rather than a fault.
    ///
    /// Read from the server's error code, not from the message. 11601 is
    /// `Interrupted`, which is what `killOp` produces; 11600 is
    /// `InterruptedAtShutdown`; 237 is `CursorKilled`. Matching on the words
    /// "interrupted" would also catch a document that happens to contain them.
    pub fn is_cancelled(&self) -> bool {
        match self {
            MongoError::Mongo(e) => matches!(code_of(e), Some(11600 | 11601 | 237)),
            _ => false,
        }
    }
}

fn code_of(e: &mongodb::error::Error) -> Option<i32> {
    match e.kind.as_ref() {
        mongodb::error::ErrorKind::Command(c) => Some(c.code),
        _ => None,
    }
}

/// The character offset `line` and `column` name in `text`, 1-based.
///
/// `serde_json` counts lines from 1 and columns in bytes from 1; the trait wants
/// one number counted in characters. The conversion matters for exactly the
/// reason the trait's does: a command document naming a collection in Chinese
/// puts three bytes under every character, and a caret placed at the byte offset
/// lands two thirds of the way through the statement.
fn offset_of(text: &str, line: usize, column: usize) -> Option<u32> {
    let mut chars = 0usize;
    for (n, current) in text.split('\n').enumerate() {
        if n + 1 == line {
            // `column` is a byte offset within this line; count the characters
            // before it rather than the bytes.
            let upto = current.len().min(column.saturating_sub(1));
            let head = current.get(..upto).unwrap_or(current);
            return u32::try_from(chars + head.chars().count() + 1).ok();
        }
        chars += current.chars().count() + 1; // the newline itself
    }
    None
}

/// A statement, as a command document.
///
/// JSON rather than a shell-like `db.orders.find(...)`. The shell's form is more
/// familiar, but it is JavaScript, and accepting a subset of JavaScript means
/// either writing a parser for it or accepting whatever the subset happens to
/// cover — which the user discovers one expression at a time. A command document
/// is the thing MongoDB's own protocol carries, so anything the server can do is
/// expressible, and an editor over it always knows whether the caret is on a key
/// or a value.
fn parse_statement(text: &str) -> Result<Document, MongoError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(MongoError::Statement {
            message: "a statement is a MongoDB command document, as in \
                      {\"find\": \"orders\", \"filter\": {\"status\": \"open\"}}"
                .to_string(),
            position: None,
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| MongoError::Statement {
            message: e.to_string(),
            position: offset_of(trimmed, e.line(), e.column()),
        })?;
    match bson::to_bson(&value) {
        Ok(Bson::Document(d)) => Ok(d),
        _ => Err(MongoError::Statement {
            message: "a statement is a command document: a JSON object whose \
                      first key is the command, as in {\"find\": \"orders\"}"
                .to_string(),
            // The text parsed as JSON and simply was not an object -- an array
            // or a bare number. There is no one place to point at.
            position: None,
        }),
    }
}

/// The command a document names, which is its first key.
fn verb(command: &Document) -> Option<&str> {
    command.keys().next().map(String::as_str)
}

/// One session against one MongoDB deployment.
pub struct MongoSource {
    client: Client,
    /// The database the connection URI named. MongoDB's namespace is
    /// deployment → database → collection, which is exactly the trait's
    /// server → schema → relation, so unlike SQL Server this database needed no
    /// level flattened away.
    default: String,
    /// Comments attached to statements now in flight, which is how `cancel`
    /// finds them again — see `cancel`.
    running: Arc<Mutex<Vec<String>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl MongoSource {
    pub async fn connect(uri: &str) -> Result<Self, MongoError> {
        let client = Client::with_uri_str(uri).await?;
        // The database from the URI path, falling back to the one MongoDB
        // itself treats as the default. A URI without a path is legitimate and
        // common -- it names a deployment, and the navigator is then a list of
        // databases rather than a list of collections.
        let default = mongodb::options::ClientOptions::parse(uri)
            .await?
            .default_database
            .unwrap_or_else(|| "test".to_string());
        // A client is lazy: `with_uri_str` resolves the URI and returns without
        // touching the network, so a wrong host would first be reported by
        // whatever call happened to be made next. Connecting means connecting.
        client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await?;
        Ok(Self {
            client,
            default,
            running: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn database(&self, name: &str) -> Database {
        self.client.database(name)
    }

    /// A label attached to a statement so `cancel` can find the operation again.
    ///
    /// MongoDB has no session-wide "stop what you are doing": `killOp` takes an
    /// opid, and an opid is only discoverable by looking the operation up in
    /// `$currentOp` while it runs. A comment is the one field that travels with
    /// the command and comes back out in that listing, so it is the handle.
    fn mark(&self) -> String {
        let n = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("dbclient-{n}")
    }

    fn began(&self, mark: &str) {
        if let Ok(mut running) = self.running.lock() {
            running.push(mark.to_string());
        }
    }

    fn ended(&self, mark: &str) {
        if let Ok(mut running) = self.running.lock() {
            running.retain(|m| m != mark);
        }
    }

    /// Runs a statement and reads enough of it to know what its columns are.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, MongoError> {
        let reader = self.start(sql, batch_rows).await?;
        Ok(ArrowStream { reader })
    }

    /// Reads a statement forward a page at a time.
    ///
    /// The trait's two properties come free here, which is worth recording after
    /// how much work they were elsewhere: a MongoDB cursor is a server-side
    /// object with a stable position by construction, so page *n* costs what
    /// page one costs and a concurrent write cannot make a document appear
    /// twice. This is the one database in the set that needed no arranging to
    /// satisfy it.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, MongoError> {
        let reader = self.start(sql, batch_rows).await?;
        let cancel = CursorCancel {
            client: self.client.clone(),
            mark: reader.mark.clone(),
        };
        Ok(Cursor { reader, cancel })
    }

    /// Runs the statement, buffers the sample, and settles the schema.
    async fn start(&self, sql: &str, batch_rows: usize) -> Result<Reader, MongoError> {
        let command = parse_statement(sql)?;
        let mark = self.mark();
        let db = self.client.database(&self.default);
        self.began(&mark);

        let is_cursor = verb(&command).is_some_and(|v| CURSOR_COMMANDS.contains(&v));
        let outcome = if is_cursor {
            self.start_cursor(db, command, mark.clone(), batch_rows)
                .await
        } else {
            self.start_command(db, command, mark.clone(), batch_rows)
                .await
        };
        if outcome.is_err() {
            self.ended(&mark);
        }
        outcome
    }

    async fn start_cursor(
        &self,
        db: Database,
        mut command: Document,
        mark: String,
        batch_rows: usize,
    ) -> Result<Reader, MongoError> {
        command.insert("comment", mark.clone());
        let mut cursor: MongoCursor<Document> = db.run_cursor_command(command).await?;

        let mut sample: VecDeque<Document> = VecDeque::new();
        let mut drained = false;
        while sample.len() < SAMPLE {
            if !cursor.advance().await? {
                drained = true;
                break;
            }
            sample.push_back(cursor.deserialize_current()?);
        }

        let shape = Shape::infer(sample.make_contiguous());
        Ok(Reader {
            cursor: if drained { None } else { Some(cursor) },
            buffered: sample,
            shape,
            batch_rows,
            produced: 0,
            running: Arc::clone(&self.running),
            mark,
        })
    }

    /// A command with no cursor: its whole reply is one document, shown as one
    /// row.
    ///
    /// `{"count": "orders"}` answers `{"n": 42, "ok": 1}` and that is genuinely
    /// the result — a one-row, two-column table. Presenting it as anything else
    /// would mean this driver deciding which fields of a reply are interesting,
    /// which is the database's business and not its client's.
    async fn start_command(
        &self,
        db: Database,
        command: Document,
        mark: String,
        batch_rows: usize,
    ) -> Result<Reader, MongoError> {
        let reply = db.run_command(command).await?;
        let sample: VecDeque<Document> = VecDeque::from([reply]);
        let shape = Shape::infer(sample.as_slices().0);
        Ok(Reader {
            cursor: None,
            buffered: sample,
            shape,
            batch_rows,
            produced: 0,
            running: Arc::clone(&self.running),
            mark,
        })
    }

    /// Asks the server to abandon whatever this session is running.
    ///
    /// Two round trips, because MongoDB offers no single call for it: find the
    /// operations carrying this session's comments in `$currentOp`, then
    /// `killOp` each opid. Both need privileges on the `admin` database, which a
    /// read-only user may not have — a cancel that is refused reports as a
    /// failure rather than silently doing nothing, since a Cancel button that
    /// lies is worse than one that says it could not.
    pub async fn cancel(&self) -> Result<(), MongoError> {
        let marks: Vec<String> = match self.running.lock() {
            Ok(running) => running.clone(),
            Err(_) => return Ok(()),
        };
        if marks.is_empty() {
            return Ok(());
        }
        kill(&self.client, &marks).await
    }
}

/// Stops the operations carrying any of `marks`.
async fn kill(client: &Client, marks: &[String]) -> Result<(), MongoError> {
    let admin = client.database("admin");
    let mut cursor: MongoCursor<Document> = admin
        .run_cursor_command(doc! {
            "aggregate": 1,
            "pipeline": [
                { "$currentOp": { "allUsers": true, "idleConnections": false } },
                { "$match": { "command.comment": { "$in": marks.to_vec() } } },
                { "$project": { "opid": 1 } },
            ],
            "cursor": {},
        })
        .await?;

    let mut opids: Vec<Bson> = Vec::new();
    while cursor.advance().await? {
        let op = cursor.deserialize_current()?;
        if let Some(opid) = op.get("opid") {
            opids.push(opid.clone());
        }
    }
    for opid in opids {
        // Best effort per the trait: an operation that finished between the
        // listing and here is not an error, it is the statement having won the
        // race. Only a refusal to try is worth reporting.
        let _ = admin.run_command(doc! { "killOp": 1, "op": opid }).await;
    }
    Ok(())
}

/// The shared machinery behind a stream and a cursor: a settled schema, the
/// documents already read to settle it, and the server cursor for the rest.
struct Reader {
    cursor: Option<MongoCursor<Document>>,
    buffered: VecDeque<Document>,
    shape: Shape,
    batch_rows: usize,
    produced: u64,
    running: Arc<Mutex<Vec<String>>>,
    mark: String,
}

impl Reader {
    fn schema(&self) -> SchemaRef {
        self.shape.schema()
    }

    async fn next(&mut self) -> Result<Option<RecordBatch>, MongoError> {
        let mut page: Vec<Document> = Vec::with_capacity(self.batch_rows);
        while page.len() < self.batch_rows {
            if let Some(document) = self.buffered.pop_front() {
                page.push(document);
                continue;
            }
            let Some(cursor) = self.cursor.as_mut() else {
                break;
            };
            match cursor.advance().await {
                Ok(true) => page.push(cursor.deserialize_current()?),
                Ok(false) => {
                    self.cursor = None;
                    break;
                }
                Err(e) => {
                    self.finish();
                    return Err(e.into());
                }
            }
        }

        if page.is_empty() {
            self.finish();
            return Ok(None);
        }
        self.produced += page.len() as u64;
        Ok(Some(self.shape.batch(&page)?))
    }

    fn finish(&mut self) {
        if let Ok(mut running) = self.running.lock() {
            running.retain(|m| m != &self.mark);
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A result being read forward in batches.
pub struct ArrowStream {
    reader: Reader,
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        self.reader.schema()
    }

    /// Documents produced, once the result has been read to the end.
    ///
    /// Rows produced rather than rows changed, which the trait allows and which
    /// is the only meaning available: a write in MongoDB answers with a reply
    /// document holding `n`, and that reply is itself this result's single row.
    /// So an `update` reports one — one row of reply — and the count the user
    /// wants is in the `n` column of it.
    pub fn rows_affected(&self) -> Option<u64> {
        (self.reader.cursor.is_none() && self.reader.buffered.is_empty())
            .then_some(self.reader.produced)
    }

    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, MongoError> {
        self.reader.next().await
    }
}

/// A result read a page at a time.
pub struct Cursor {
    reader: Reader,
    cancel: CursorCancel,
}

impl Cursor {
    pub fn schema(&self) -> SchemaRef {
        self.reader.schema()
    }

    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, MongoError> {
        self.reader.next().await
    }

    pub fn canceller(&self) -> CursorCancel {
        self.cancel.clone()
    }

    pub async fn close(&mut self) -> Result<(), MongoError> {
        // Dropping the server cursor is what releases it; the driver sends
        // `killCursors` when the handle goes away. Taking it here makes that
        // happen at the moment the caller chose.
        self.reader.cursor = None;
        self.reader.buffered.clear();
        self.reader.finish();
        Ok(())
    }
}

/// Stops the fetch one cursor is running.
#[derive(Clone)]
pub struct CursorCancel {
    client: Client,
    mark: String,
}

impl CursorCancel {
    pub async fn cancel(&self) -> Result<(), MongoError> {
        if self.mark.is_empty() {
            return Ok(());
        }
        kill(&self.client, std::slice::from_ref(&self.mark)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_that_is_not_json_says_where_it_stopped() {
        let err = parse_statement("SELECT * FROM orders").expect_err("not a command document");
        let at = err.statement_position().expect("a position");
        assert!(at >= 1, "positions count from one, got {at}");
    }

    #[test]
    fn a_position_is_counted_in_characters_and_not_bytes() {
        // The check that proves the conversion happens at all. Every character
        // before the break is three bytes, so a driver reporting the byte offset
        // would put the caret past the end of the statement.
        let statement = "{\"find\": \"訂單訂單訂單\" oops}";
        let err = parse_statement(statement).expect_err("a trailing word is not JSON");
        let at = err.statement_position().expect("a position") as usize;
        assert!(
            at <= statement.chars().count() + 1,
            "position {at} is past the end of a {}-character statement",
            statement.chars().count()
        );
        assert!(at < statement.len(), "a byte offset would be larger");
    }

    #[test]
    fn a_statement_that_is_json_but_not_an_object_says_what_one_looks_like() {
        let err = parse_statement("[1, 2, 3]").expect_err("an array is not a command");
        assert!(err.to_string().contains("find"), "got: {err}");
    }

    #[test]
    fn an_empty_statement_is_refused_before_the_server_is_troubled() {
        assert!(parse_statement("   ").is_err());
    }

    #[test]
    fn the_command_is_the_documents_first_key() {
        let command = parse_statement(r#"{"find": "orders", "filter": {}}"#).expect("parses");
        assert_eq!(verb(&command), Some("find"));
        assert!(CURSOR_COMMANDS.contains(&verb(&command).unwrap()));
    }

    #[test]
    fn a_command_with_no_cursor_is_recognised_as_such() {
        let command = parse_statement(r#"{"count": "orders"}"#).expect("parses");
        assert!(!CURSOR_COMMANDS.contains(&verb(&command).unwrap()));
    }

    #[test]
    fn an_offset_on_a_later_line_counts_the_lines_before_it() {
        let text = "{\n  \"find\": oops\n}";
        let at = offset_of(text, 2, 11).expect("an offset");
        assert_eq!(&text[at as usize - 1..at as usize], "o");
    }
}
