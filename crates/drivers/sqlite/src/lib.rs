//! SQLite read path: open a database file, execute, stream Arrow record batches.
//!
//! The second driver, and deliberately the one least like the first. SQLite is a
//! library rather than a server: its API is blocking, a connection is a file
//! handle rather than a session on another machine, there is no server-side
//! cursor to declare, and its catalog is read through pragmas instead of
//! queried. Phase 1 left the `Driver` trait unwritten on the grounds that an
//! abstraction over one implementation is invented rather than derived. This is
//! the second implementation, so it stands on its own and the trait comes next.
//!
//! The surface mirrors `driver-postgres` where the two genuinely agree, and says
//! so where they do not, because those differences are the evidence the trait
//! gets derived from.

mod arrow_map;
mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::datatypes::{Field, Schema, SchemaRef};
use arrow_map::{ColBuilder, ColumnType};
use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationshipInfo, SchemaInfo, TriggerInfo,
};
use rusqlite::{Connection, InterruptHandle, OpenFlags, Row};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("no database file at {0}")]
    NoSuchDatabase(String),
    #[error("no schema named {0} is open on this connection")]
    NoSuchSchema(String),
    #[error("column {column:?} holds {found} where the column reads as {expected}")]
    TypeMismatch {
        column: String,
        found: &'static str,
        expected: &'static str,
    },
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("the thread this statement was running on stopped before it said why")]
    ReaderGone,
}

impl SqliteError {
    /// Where in the statement SQLite says the trouble is: a 1-based index,
    /// counted in characters, into the SQL that was sent.
    ///
    /// The same contract PostgreSQL's driver reports, which takes converting:
    /// SQLite counts bytes from zero and PostgreSQL counts characters from one.
    /// Handing a byte offset to a front end that expects a character index puts
    /// the caret past the error on the first line containing anything outside
    /// ASCII, and the difference does not show up in testing done in English.
    pub fn statement_position(&self) -> Option<u32> {
        let SqliteError::Sqlite(rusqlite::Error::SqlInputError { sql, offset, .. }) = self else {
            return None;
        };
        let offset = usize::try_from(*offset).ok()?;
        // `get` rather than slicing: an offset that is not a character boundary
        // would panic, and no position is better than a crash.
        let characters = sql.get(..offset)?.chars().count();
        u32::try_from(characters + 1).ok()
    }

    /// Whether this statement stopped because somebody asked it to.
    ///
    /// Read from the error code SQLite raised rather than from this side
    /// remembering that it called `cancel`, for the reason the PostgreSQL driver
    /// gives: a statement can fail on its own merits in the same moment the
    /// request lands, and reporting that as "Cancelled" hides a real fault
    /// behind a button.
    pub fn is_cancelled(&self) -> bool {
        let SqliteError::Sqlite(e) = self else {
            return false;
        };
        e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
    }
}

/// Every interrupt handle this session currently has work running on.
///
/// Shared rather than owned, because the same handle has two holders: the
/// registry `cancel` walks, and the result that hands one out through its own
/// canceller. `InterruptHandle` is not itself clonable.
type Cancels = Arc<Mutex<Vec<(u64, Arc<InterruptHandle>)>>>;

/// One SQLite database file, and the connections opened against it.
///
/// There is no pool. PostgreSQL keeps one because a connection there is a TCP
/// session and a process on the server, expensive enough that handing it back is
/// worth the bookkeeping; here it is an open file, so a caller that needs one
/// opens one. What survives from that design is the reason behind it — a result
/// being read must not make the navigator wait — and it survives more simply,
/// because every reader has a connection to itself by construction.
pub struct SqliteSource {
    path: PathBuf,
    cancels: Cancels,
    next_id: AtomicU64,
}

/// Keeps one connection's interrupt handle reachable by `cancel` for exactly as
/// long as there is something on it to interrupt.
///
/// Registration is scoped rather than permanent, which is the opposite of the
/// PostgreSQL driver's arrangement. There, a connection outlives the statement
/// and is reused, so its cancel token is registered once at the point it is
/// opened. Here a connection is opened for one piece of work and closed after
/// it, and interrupting a handle whose connection has already been dropped is
/// not something to leave lying in a list.
struct Registration {
    id: u64,
    cancels: Cancels,
}

impl Registration {
    /// Puts `handle` where `cancel` can find it, and takes it back out when the
    /// returned value is dropped.
    fn hold(cancels: Cancels, id: u64, handle: Arc<InterruptHandle>) -> Self {
        if let Ok(mut held) = cancels.lock() {
            held.push((id, handle));
        }
        Self { id, cancels }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.retain(|(id, _)| *id != self.id);
        }
    }
}

/// Opens one connection to the database file.
///
/// Deliberately without `SQLITE_OPEN_CREATE`. A client that creates a database
/// from a mistyped path answers a connection mistake with an empty database,
/// which looks like success and is the harder error to notice — the user goes
/// looking for their tables rather than for their typo.
fn open(path: &Path) -> Result<Connection, SqliteError> {
    // Checked here so the message can name the path. SQLite's own refusal is
    // "unable to open database file", which is every reason at once and does not
    // say which file it meant.
    if !path.is_file() {
        return Err(SqliteError::NoSuchDatabase(path.display().to_string()));
    }
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?)
}

/// Runs `work` on a thread that is allowed to block, and treats a panic in it as
/// one.
///
/// Every call into SQLite blocks, so none of them may run on a runtime worker.
async fn blocking<T, F>(work: F) -> Result<T, SqliteError>
where
    F: FnOnce() -> Result<T, SqliteError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(_) => Err(SqliteError::ReaderGone),
    }
}

impl SqliteSource {
    pub async fn connect(path: &str) -> Result<Self, SqliteError> {
        let path = PathBuf::from(path);
        let probe = path.clone();
        blocking(move || {
            let conn = open(&probe)?;
            // Opening does not read the file — SQLite defers that to the first
            // statement — so a path that is not a database would be accepted
            // here and fail later, with the connection dialog long gone and
            // nothing on screen to correct.
            conn.pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))?;
            Ok(())
        })
        .await?;

        Ok(Self {
            path,
            cancels: Arc::new(Mutex::new(Vec::new())),
            next_id: AtomicU64::new(0),
        })
    }

    /// Asks SQLite to abandon whatever this session is currently running.
    ///
    /// Not async, and the difference from PostgreSQL is worth stating: there a
    /// cancel is a request that travels to the server on a connection of its
    /// own, so it is something to await. Here it sets a flag the running
    /// statement checks, in this process, and there is nothing to wait for.
    ///
    /// Best-effort in the same way, though. SQLite ignores an interrupt raised
    /// while nothing is running, and a statement that finishes before the flag
    /// is read finishes normally — so success means the flag was set, not that
    /// anything stopped. What actually happened shows up as the statement
    /// failing with `is_cancelled`, or not failing at all.
    pub fn cancel(&self) {
        let Ok(cancels) = self.cancels.lock() else {
            return;
        };
        for (_, handle) in cancels.iter() {
            handle.interrupt();
        }
    }

    fn register(&self, handle: Arc<InterruptHandle>) -> Registration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Registration::hold(Arc::clone(&self.cancels), id, handle)
    }

    /// Runs one piece of catalog work on a connection opened for it.
    ///
    /// A connection per call, where PostgreSQL takes one from a pool. Opening a
    /// file is cheap enough that the pool would be bookkeeping for its own sake,
    /// and the property the pool was there to protect — a navigator that does not
    /// queue behind a result being read — comes for free once nothing is shared.
    ///
    /// The connection is registered for the length of the call so that `cancel`
    /// reaches it. Most of these answer in microseconds and will never be
    /// cancelled; `referenced_by` reads every table's keys, and on a schema with
    /// thousands of them it is the one that a person waits for.
    async fn with_connection<T, F>(&self, work: F) -> Result<T, SqliteError>
    where
        F: FnOnce(&Connection) -> Result<T, SqliteError> + Send + 'static,
        T: Send + 'static,
    {
        let path = self.path.clone();
        let cancels = Arc::clone(&self.cancels);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        blocking(move || {
            let conn = open(&path)?;
            let _registered =
                Registration::hold(cancels, id, Arc::new(conn.get_interrupt_handle()));
            work(&conn)
        })
        .await
    }

    /// Runs schema-scoped catalog work, having first established that the schema
    /// is there to be read.
    async fn with_schema<T, F>(&self, schema: &str, work: F) -> Result<T, SqliteError>
    where
        F: FnOnce(&Connection) -> Result<T, SqliteError> + Send + 'static,
        T: Send + 'static,
    {
        let schema = schema.to_string();
        self.with_connection(move |conn| {
            metadata::require_schema(conn, &schema)?;
            work(conn)
        })
        .await
    }

    /// The databases attached to this connection, for the navigator root.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, SqliteError> {
        self.with_connection(metadata::schemas).await
    }

    /// Tables, views, and virtual tables within a schema.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, SqliteError> {
        let owned = schema.to_string();
        self.with_schema(schema, move |conn| metadata::relations(conn, &owned))
            .await
    }

    /// Column definitions for one relation.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, SqliteError> {
        let (owned, relation) = (schema.to_string(), relation.to_string());
        self.with_schema(schema, move |conn| {
            metadata::columns(conn, &owned, &relation)
        })
        .await
    }

    /// The statement a view is defined by; `None` for a relation that has none.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, SqliteError> {
        let (owned, relation) = (schema.to_string(), relation.to_string());
        self.with_schema(schema, move |conn| {
            metadata::definition(conn, &owned, &relation)
        })
        .await
    }

    /// Indexes on one relation.
    pub async fn indexes(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<IndexInfo>, SqliteError> {
        let (owned, relation) = (schema.to_string(), relation.to_string());
        self.with_schema(schema, move |conn| {
            metadata::indexes(conn, &owned, &relation)
        })
        .await
    }

    /// Foreign keys declared by one relation.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, SqliteError> {
        let (owned, relation) = (schema.to_string(), relation.to_string());
        self.with_schema(schema, move |conn| {
            metadata::foreign_keys(conn, &owned, &relation)
        })
        .await
    }

    /// Foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, SqliteError> {
        let (owned, relation) = (schema.to_string(), relation.to_string());
        self.with_schema(schema, move |conn| {
            metadata::referenced_by(conn, &owned, &relation)
        })
        .await
    }

    /// UNIQUE constraints. SQLite records no CHECK constraint outside the DDL
    /// text, so none is reported — see `metadata`.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, SqliteError> {
        let (owned, relation) = (schema.to_string(), relation.to_string());
        self.with_schema(schema, move |conn| {
            metadata::constraints(conn, &owned, &relation)
        })
        .await
    }

    /// Triggers on one relation, with the statement each was created from.
    pub async fn triggers(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<TriggerInfo>, SqliteError> {
        let (owned, relation) = (schema.to_string(), relation.to_string());
        self.with_schema(schema, move |conn| {
            metadata::triggers(conn, &owned, &relation)
        })
        .await
    }

    /// Prepare `sql` and begin streaming results as Arrow batches of
    /// `batch_rows` rows.
    ///
    /// Resolves once the first row is available, or once the statement has
    /// finished having produced none. That is later than PostgreSQL's `query`
    /// promises and for a different reason: there the prepared statement
    /// describes its own columns, here a column's type is not always knowable
    /// without seeing a value — see `arrow_map` — so the schema this returns has
    /// to be paid for with the first row.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, SqliteError> {
        self.stream(sql, batch_rows).await
    }

    /// Open a cursor over `sql` and return a handle to fetch pages.
    ///
    /// The same thing as `query`, and that is the finding rather than a
    /// shortcut. What Phase 1 wanted from a cursor was that page two agrees with
    /// page one, and PostgreSQL needs `DECLARE CURSOR` to get it because
    /// otherwise each page is a statement of its own with a snapshot of its own.
    /// SQLite holds a read transaction from a statement's first step until its
    /// last, so a statement stepped forward already is the cursor, and there is
    /// nothing left for this to add.
    ///
    /// Deliberately no `BEGIN` around it. That would hold a read lock for as long
    /// as somebody leaves a table browser open, which in the default journal mode
    /// is enough to refuse every write to the database — a real cost, paid for a
    /// guarantee already in hand.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, SqliteError> {
        let stream = self.stream(sql, batch_rows).await?;
        Ok(Cursor { stream })
    }

    async fn stream(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, SqliteError> {
        // A zero-row batch would be emitted forever without moving the statement
        // forward, so the one value that cannot mean anything is read as the
        // smallest one that can.
        let batch_rows = batch_rows.max(1);
        let path = self.path.clone();
        let conn = blocking(move || open(&path)).await?;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        let registration = self.register(Arc::clone(&interrupt));

        let (schema_tx, schema_rx) = oneshot::channel();
        // One batch in flight. The reader thread blocks on a full channel, so
        // the result stops growing when the front end stops reading it — the
        // bound Phase 1 asked for, expressed as backpressure rather than as an
        // eviction policy.
        let (batch_tx, batches) = mpsc::channel(1);
        let rows_affected = Arc::new(AtomicI64::new(-1));

        let sql = sql.to_string();
        let affected = Arc::clone(&rows_affected);
        // A thread of its own rather than tokio's blocking pool. A result stays
        // open for as long as a front end holds it — a scroll position is not a
        // task that finishes — and a pool worker parked on one is a worker
        // nothing else can have.
        std::thread::spawn(move || {
            pump(conn, &sql, batch_rows, schema_tx, batch_tx, &affected);
        });

        let schema = schema_rx.await.map_err(|_| SqliteError::ReaderGone)??;
        Ok(ArrowStream {
            schema,
            batches,
            rows_affected,
            interrupt,
            _registration: registration,
        })
    }
}

/// A result being read forward, one batch at a time.
pub struct ArrowStream {
    schema: SchemaRef,
    batches: mpsc::Receiver<Result<RecordBatch, SqliteError>>,
    rows_affected: Arc<AtomicI64>,
    interrupt: Arc<InterruptHandle>,
    /// Dropped with the stream, which is what takes this result's connection out
    /// of `cancel`'s reach once there is nothing left on it to cancel.
    _registration: Registration,
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows the statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Two different numbers under one name, as PostgreSQL also reports: rows
    /// changed for a statement that writes, rows produced for one that reads.
    /// SQLite will answer `changes()` for a `SELECT` too, but the number it
    /// gives back is left over from whatever ran before — a count belonging to
    /// another statement is worse than no count, so the statement is asked
    /// whether it writes at all before that number is believed.
    pub fn rows_affected(&self) -> Option<u64> {
        let n = self.rows_affected.load(Ordering::Acquire);
        u64::try_from(n).ok()
    }

    /// Next batch, or `None` once the result is fully consumed.
    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, SqliteError> {
        match self.batches.recv().await {
            Some(batch) => batch.map(Some),
            // The sender is gone, which is how the reader thread says it reached
            // the end. An error would have arrived through the channel first.
            None => Ok(None),
        }
    }
}

/// A cursor over a SQLite query result.
///
/// Holds a connection and a read transaction for as long as it lives, so that
/// the pages it hands out are pages of one result rather than of a database
/// that kept changing underneath.
pub struct Cursor {
    stream: ArrowStream,
}

/// Asks SQLite to stop what a cursor is fetching.
///
/// Separate from the cursor so the two can be held at once: cancelling means
/// reaching the connection while a fetch has it, which is the whole situation.
pub struct CursorCancel {
    interrupt: Arc<InterruptHandle>,
}

impl CursorCancel {
    /// Raised is not interrupted, as with `SqliteSource::cancel`: a fetch that
    /// had already finished leaves nothing to stop, and what actually happened
    /// shows up as the fetch failing with `is_cancelled`.
    pub fn cancel(&self) {
        self.interrupt.interrupt();
    }
}

impl Cursor {
    /// Fetch the next page, or `None` once the cursor has reached the end.
    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, SqliteError> {
        self.stream.next_batch().await
    }

    /// The columns this cursor's rows arrive in.
    pub fn schema(&self) -> SchemaRef {
        self.stream.schema()
    }

    /// A handle for stopping this cursor's fetch from another thread.
    pub fn canceller(&self) -> CursorCancel {
        CursorCancel {
            interrupt: Arc::clone(&self.stream.interrupt),
        }
    }

    /// Close the cursor explicitly.
    ///
    /// Optional: dropping it does the same thing. Closing the channel is what
    /// tells the reader thread to stop, and the thread ending is what closes the
    /// connection and with it the statement's read transaction.
    pub async fn close(&mut self) -> Result<(), SqliteError> {
        self.stream.batches.close();
        // Drained rather than merely closed, so the reader is not left blocked
        // on a send nobody will ever take.
        while self.stream.batches.recv().await.is_some() {}
        Ok(())
    }
}

/// Reads one statement to the end, sending its schema and then its batches.
///
/// Runs on a thread of its own for the life of the result. Everything it needs
/// is owned: the connection, the statement prepared from it, and the rows
/// stepped out of that — three borrows that cannot cross an await, which is the
/// reason this is a thread and not a task.
fn pump(
    conn: Connection,
    sql: &str,
    batch_rows: usize,
    schema_tx: oneshot::Sender<Result<SchemaRef, SqliteError>>,
    batch_tx: mpsc::Sender<Result<RecordBatch, SqliteError>>,
    rows_affected: &AtomicI64,
) {
    let mut schema_tx = Some(schema_tx);
    let result = read(
        &conn,
        sql,
        batch_rows,
        &mut schema_tx,
        &batch_tx,
        rows_affected,
    );
    if let Err(e) = result {
        // Which way the failure goes out depends on how far this got: a
        // statement that never produced a schema failed at `query`, and one that
        // did fails at the batch the caller is waiting for. Both are dropped
        // silently if nobody is listening any more, which is the ordinary end of
        // a result the front end let go of.
        match schema_tx.take() {
            Some(tx) => drop(tx.send(Err(e))),
            None => drop(batch_tx.blocking_send(Err(e))),
        }
    }
}

fn read(
    conn: &Connection,
    sql: &str,
    batch_rows: usize,
    schema_tx: &mut Option<oneshot::Sender<Result<SchemaRef, SqliteError>>>,
    batch_tx: &mpsc::Sender<Result<RecordBatch, SqliteError>>,
    rows_affected: &AtomicI64,
) -> Result<(), SqliteError> {
    let mut stmt = conn.prepare(sql)?;
    let (names, declared): (Vec<String>, Vec<Option<String>>) = stmt
        .columns()
        .iter()
        .map(|c| (c.name().to_string(), c.decl_type().map(str::to_string)))
        .collect();
    // Asked before the statement runs, because afterwards `changes()` reports a
    // number belonging to whatever wrote last.
    let reads_only = stmt.readonly();

    let mut rows = stmt.query([])?;
    let mut produced: i64 = 0;
    let mut in_batch = 0usize;

    // The first row settles the schema and is appended in the same borrow. It
    // cannot be held across the next step — a stepped row is only valid until
    // the step after it — so everything that needs it happens here.
    let (schema, types, mut builders) = {
        let first = rows.next()?;
        let types = arrow_map::resolve(&declared, first)?;
        let fields: Vec<Field> = names
            .iter()
            .zip(&types)
            .map(|(name, t)| Field::new(name, t.data_type(), true))
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let mut builders = new_builders(&types, batch_rows);
        if let Some(row) = first {
            append(&mut builders, &names, row)?;
            produced += 1;
            in_batch += 1;
        }
        (schema, types, builders)
    };

    let sent = schema_tx
        .take()
        .expect("the schema is sent exactly once, here")
        .send(Ok(Arc::clone(&schema)));
    if sent.is_err() {
        // Nobody is waiting for this result any more. Not an error: it is what
        // a front end dropping a query before its first batch looks like.
        return Ok(());
    }

    loop {
        if in_batch == batch_rows {
            if !emit(&mut builders, &types, batch_rows, &schema, batch_tx)? {
                return Ok(());
            }
            in_batch = 0;
        }
        let Some(row) = rows.next()? else { break };
        append(&mut builders, &names, row)?;
        produced += 1;
        in_batch += 1;
    }

    if in_batch > 0 {
        emit(&mut builders, &types, batch_rows, &schema, batch_tx)?;
    }

    let affected = if reads_only {
        produced
    } else {
        i64::try_from(conn.changes()).unwrap_or(i64::MAX)
    };
    rows_affected.store(affected, Ordering::Release);
    Ok(())
}

fn new_builders(types: &[ColumnType], batch_rows: usize) -> Vec<ColBuilder> {
    types
        .iter()
        .map(|t| ColBuilder::new(*t, batch_rows))
        .collect()
}

fn append(builders: &mut [ColBuilder], names: &[String], row: &Row<'_>) -> Result<(), SqliteError> {
    for (idx, builder) in builders.iter_mut().enumerate() {
        builder.append(&names[idx], row, idx)?;
    }
    Ok(())
}

/// Sends one batch, answering whether anyone was still there to take it.
///
/// Builders are replaced rather than reused, as in the PostgreSQL driver: an
/// Arrow builder hands its buffer away on `finish`, and reusing one would mean
/// copying out of a buffer the batch already owns.
fn emit(
    builders: &mut Vec<ColBuilder>,
    types: &[ColumnType],
    batch_rows: usize,
    schema: &SchemaRef,
    batch_tx: &mpsc::Sender<Result<RecordBatch, SqliteError>>,
) -> Result<bool, SqliteError> {
    let arrays = builders.iter_mut().map(|b| b.finish()).collect();
    *builders = new_builders(types, batch_rows);
    let batch = RecordBatch::try_new(Arc::clone(schema), arrays)?;
    Ok(batch_tx.blocking_send(Ok(batch)).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no database — it needs the absence of one, which is why it can run
    /// in the unit suite.
    #[tokio::test]
    async fn a_path_with_no_database_behind_it_says_which_path() {
        let err = SqliteSource::connect("/nonexistent/directory/library.db")
            .await
            .err()
            .expect("nothing is at that path");
        let message = err.to_string();
        // SQLite's own refusal is "unable to open database file", which fits
        // every reason at once and never says which file it meant.
        assert!(
            message.contains("library.db"),
            "the message should name the path that failed, got: {message}"
        );
    }

    /// A connection that would have been created, had this been allowed to.
    #[tokio::test]
    async fn a_missing_file_is_not_quietly_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typo.db");
        assert!(SqliteSource::connect(path.to_str().unwrap()).await.is_err());
        // The failure mode this guards: a client that answers a mistyped path
        // with an empty database looks like it connected, and the user goes
        // looking for their tables instead of for their typo.
        assert!(!path.exists(), "connecting must not create the database");
    }
}
