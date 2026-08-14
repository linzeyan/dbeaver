//! What every driver is, from the outside.
//!
//! Phase 1 deliberately left this unwritten. With one driver a trait is invented
//! rather than derived, and an invented abstraction over a database is paid for
//! in every driver that has to pretend to fit it. It is written now because
//! there are two implementations to derive it from, and the two were chosen to
//! disagree: PostgreSQL is a server reached over a socket, SQLite is a library
//! in this process, and almost every assumption the first one made turned out to
//! be about servers rather than about databases.
//!
//! What survived the second implementation:
//!
//! **Everything is fallible in the same way.** A front end needs three things
//! from a failure — what to print, where in the statement to put the caret, and
//! whether this was a fault or a button the user pressed. Drivers keep their own
//! rich error types internally and convert at this boundary, so the FFI has one
//! error and not one per database.
//!
//! **Cancellation is asynchronous even where it is not.** PostgreSQL sends the
//! request to the server on a connection of its own, so it is something to wait
//! for; SQLite sets a flag in this process and returns. The trait takes the
//! shape the slower one needs, because a synchronous signature would have no
//! room for the round trip and every driver that talks to a server would have to
//! block a runtime thread inside it.
//!
//! **A cursor is not always a `DECLARE`.** What Phase 1 wanted from one was that
//! page two agrees with page one and that paging does not re-read. PostgreSQL
//! needs a server-side cursor for that; SQLite gets it from a statement stepped
//! forward. So the trait asks for the property and not for the mechanism.
//!
//! **A canceller is taken out in advance.** Cancelling a fetch means reaching
//! the connection at the moment the fetch has it, so it cannot be something
//! reached for through the cursor. Both drivers arrived at the same shape
//! independently, which is the strongest evidence in here that it is the right
//! one.
//!
//! Six more implementations later — MySQL, SQL Server, DuckDB, ClickHouse and,
//! deliberately, MongoDB — one thing changed: `query` and `cursor` take a
//! `statement` and not a `sql`. That is the whole of it. A document database
//! whose statements are JSON objects, an analytical database with no cursors of
//! its own, and an embedded one that runs in this thread all fit without the
//! trait growing a method, an option or an escape hatch. Where the databases
//! genuinely disagree — whether a failure carries a position, whether reading a
//! relation that is not there is a failure at all — the trait already said "or
//! not", so the disagreement had somewhere to live.

mod metadata;

pub use metadata::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo,
};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use std::fmt;

/// A failure, reduced to what a front end acts on.
///
/// Three fields, because there are three questions: what to show, where to put
/// the caret, and whether to show it as a fault at all. Drivers keep whatever
/// structure their database gives them and answer these on the way out.
#[derive(Debug, Clone)]
pub struct DbError {
    message: String,
    position: Option<u32>,
    cancelled: bool,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            position: None,
            cancelled: false,
        }
    }

    /// Where in the statement the database says the trouble is: 1-based, counted
    /// in characters, into the text that was sent.
    ///
    /// Characters and not bytes, and from one and not zero, because the two
    /// databases already disagreed about both and a front end cannot ask which
    /// it is holding. SQLite reports a byte offset from zero and converts here;
    /// PostgreSQL reports this contract natively. The difference is invisible
    /// until somebody names a table in a language that is not English.
    pub fn at_position(mut self, position: Option<u32>) -> Self {
        self.position = position;
        self
    }

    /// Marks a failure as something somebody asked for.
    ///
    /// Kept apart from the message because it is a fact about the failure rather
    /// than a way of describing it: "canceling statement due to user request" in
    /// an error banner reads as a fault, when it is the button they just pressed
    /// working.
    pub fn as_cancelled(mut self, cancelled: bool) -> Self {
        self.cancelled = cancelled;
        self
    }

    pub fn statement_position(&self) -> Option<u32> {
        self.position
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

pub type DbResult<T> = Result<T, DbError>;

/// One session against one database.
///
/// A session, not a connection: an implementation is free to hold several, and
/// both of the first two do. What the caller is promised is that a result being
/// read does not make the navigator wait behind it — PostgreSQL keeps a pool for
/// that, SQLite opens a file handle per reader, and neither arrangement is
/// visible from here.
#[async_trait]
pub trait Driver: Send + Sync {
    // ---- Metadata -------------------------------------------------------

    /// The navigator root. A database with no schema layer of its own answers
    /// with the one container it has rather than with an empty list, so that the
    /// navigator has the same shape everywhere.
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>>;

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>>;

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>>;

    /// The statement a view is defined by; `None` for a relation that has none.
    ///
    /// How faithful that statement is differs by database and cannot be made
    /// uniform: SQLite returns the text it was given, PostgreSQL renders the
    /// query back from its parse tree and so returns a normalized body without
    /// the `CREATE VIEW` around it. Both are the definition as that database
    /// holds it, which is the most this can promise.
    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>>;

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>>;

    /// Foreign keys this relation declares.
    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>>;

    /// Foreign keys other relations declare against this one.
    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>>;

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>>;

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>>;

    // ---- Results --------------------------------------------------------

    /// Run `statement` and return a stream over its results, in batches of at
    /// most `batch_rows` rows.
    ///
    /// `statement` is text this database understands, and not necessarily SQL.
    /// The parameter was called `sql` while PostgreSQL and SQLite were the only
    /// implementations, and MongoDB is what showed the word to be wrong: it
    /// takes a command document, `{"find": "orders", "filter": {…}}`, which is
    /// what its own protocol carries. Nothing about the shape of this call had
    /// to change to accommodate that, which is the useful result — a statement
    /// really is just text the database understands, and only the name said
    /// otherwise.
    ///
    /// When this resolves relative to the statement running is deliberately not
    /// specified, because the two implementations already differ and neither is
    /// wrong: PostgreSQL waits out the whole command because the server buffers
    /// its output, SQLite waits for the first row because that is when a column's
    /// type is settled. What is specified is that an execution failure may
    /// surface from `next_batch` rather than from here, so a caller that stops at
    /// a successful `query` has not established that the statement worked.
    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>>;

    /// Read `statement` forward, a page at a time.
    ///
    /// Two properties, which is all this asks for — the mechanism is the
    /// driver's. Reading page *n* must not re-read the pages before it, so page
    /// *n* costs what page one costs. And the pages must agree with each other:
    /// a write landing between two of them must not make a row appear twice or
    /// not at all. Neither is achievable with `LIMIT`/`OFFSET`, which is what
    /// this exists instead of.
    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn Cursor>>;

    /// Ask the database to abandon whatever this session is running.
    ///
    /// May be called while another call is in flight on the same session, and
    /// has to be: everything else here blocks, so a cancel that waited its turn
    /// would arrive after the statement it exists to interrupt.
    ///
    /// Best-effort. Success means the request was delivered, not that anything
    /// stopped — the statement may have finished first, or there may have been
    /// nothing running. What actually happened shows up where the statement is,
    /// as a failure whose `is_cancelled` is true.
    ///
    /// Does not reach a cursor. A cursor is handed out to be held and outlives
    /// the call that made it, so it carries its own canceller.
    async fn cancel(&self) -> DbResult<()>;

    // ---- Transactions ---------------------------------------------------

    /// Whether statements on this session can be wrapped in a transaction.
    ///
    /// No default. A driver that cannot has to say so out loud, because the
    /// front end hides Commit and Rollback rather than offering buttons that
    /// fail — and because "false" here means two quite different things that
    /// both deserve a sentence in the implementation: a database with no
    /// transactions, and a driver whose statements do not yet share one
    /// connection to hold one.
    ///
    /// A transaction is a property of a connection, so this is really a question
    /// about the arrangement inside the driver rather than about the database.
    /// It does not cover cursors: a cursor runs on a connection of its own and
    /// is outside whatever the session has open.
    fn transactional(&self) -> bool;

    /// Take one step of transaction control, on the connection statements run
    /// on.
    ///
    /// That connection and no other. A `BEGIN` sent down a pooled connection
    /// opens a transaction on a connection the next statement will not be given,
    /// and nothing afterwards can commit it.
    ///
    /// A step this database does not have is refused rather than skipped. SQL
    /// Server releases savepoints implicitly and DuckDB has none at all, and a
    /// client that quietly did nothing would leave somebody believing there is a
    /// point they can come back to.
    async fn transaction(&self, step: &TxStep) -> DbResult<()>;
}

/// One step of transaction control.
///
/// An enum rather than six methods on the trait. Seven drivers implement this,
/// and a method apiece would be six more places for one of them to be quietly
/// forgotten — a compiler insists on a method existing, never on it doing
/// anything. One `match` per driver puts every answer, including "this database
/// cannot do that", where a reader can check them together.
///
/// The names in the three savepoint steps are generated by whoever holds the
/// session, and are always letters, digits and underscores. Drivers write them
/// into a statement, which is only safe because of that: a savepoint name is an
/// identifier and no database takes one as a parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStep {
    Begin,
    Commit,
    Rollback,
    /// A point to come back to, inside the transaction already open.
    Savepoint(String),
    /// Undo back to a savepoint, leaving the transaction open.
    RollbackTo(String),
    /// Forget a savepoint, keeping everything done since.
    Release(String),
}

/// A result being read forward in batches.
#[async_trait]
pub trait ResultStream: Send {
    /// The columns the rows arrive in, known before the first batch.
    fn schema(&self) -> SchemaRef;

    /// Rows the statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Negative space matters here: zero is a real answer — an `UPDATE` that
    /// matched nothing — so "not known yet" has to be something else. What the
    /// number counts differs with the statement: rows changed for one that
    /// writes, rows produced for one that reads.
    fn rows_affected(&self) -> Option<u64>;

    /// Next batch, or `None` once the result is fully consumed.
    async fn next_batch(&mut self) -> DbResult<Option<RecordBatch>>;
}

/// A result read a page at a time, with a stable position.
#[async_trait]
pub trait Cursor: Send {
    fn schema(&self) -> SchemaRef;

    /// Next page, or `None` once the cursor has reached the end.
    async fn fetch(&mut self) -> DbResult<Option<RecordBatch>>;

    /// A handle for stopping this cursor's fetch from another thread.
    ///
    /// Taken out in advance rather than reached for at cancel time, because by
    /// then the cursor itself is borrowed by the fetch that is to be stopped —
    /// which is the whole situation.
    fn canceller(&self) -> Box<dyn CursorCancel>;

    /// Close the cursor and release whatever it was holding.
    ///
    /// Optional: dropping it does the same. This exists for a front end that
    /// wants to close at a moment of its choosing rather than whenever the last
    /// reference goes away — which on macOS can be the main thread.
    async fn close(&mut self) -> DbResult<()>;
}

/// Stops the fetch one cursor is running.
#[async_trait]
pub trait CursorCancel: Send + Sync {
    /// Delivered is not interrupted, as with `Driver::cancel`: a fetch that had
    /// already finished leaves nothing to stop and this still succeeds.
    async fn cancel(&self) -> DbResult<()>;
}
