//! DuckDB read path: open a database file, execute, stream Arrow record batches.
//!
//! The second embedded driver, and most of what `driver-sqlite` learned about
//! driving a blocking library from async code transfers without a word changed —
//! a thread per result, a `oneshot` for the schema, an `mpsc::channel(1)` for
//! backpressure, a scoped registration so `cancel` can reach a connection for
//! exactly as long as there is something on it to interrupt. What does not
//! transfer is written down where it happens, because those differences are the
//! evidence the `Driver` trait gets revised from.
//!
//! Three of them shape everything here.
//!
//! **DuckDB produces the Arrow.** There is no column builder and no per-value
//! decode: one `step()` hands over a DuckDB data chunk already converted to an
//! Arrow `StructArray`. That is worth one columnar copy inside DuckDB — the data
//! chunk is destroyed before the array is read, so the buffers cannot be aliases
//! of it — and no copy at all from there into Swift. What is left of a type map
//! is in `arrow_map`, and it is an audit rather than a conversion.
//!
//! **A batch is a DuckDB data chunk, and a data chunk is at most 2048 rows.**
//! `STANDARD_VECTOR_SIZE` is a compile-time constant; the C API exposes only a
//! read-only `duckdb_vector_size()`. So `batch_rows` is honoured downward, by
//! slicing — which costs nothing, a slice shares its buffers — and cannot be
//! honoured upward: asking for 65536 rows a batch gets 2048, because reaching
//! the larger number means `concat_batches` copying every buffer, which is the
//! cost this path exists to avoid.
//!
//! **The crate's own iterator panics on a fetch error**, and a fetch error is
//! the normal outcome of pressing Cancel. `Arrow::next` turns the `Err` arm of
//! `step()` into `panic!`, and the release profile is `panic = "abort"`, so a
//! driver built the way every example in the crate is written would answer the
//! Cancel button by killing the application. `stream_arrow` is still called —
//! it is what puts the statement into streaming execution — and its return value
//! is dropped unused; the pump reads `Statement::step()` off the same statement.

mod arrow_map;
mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow_map::Layout;
use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationshipInfo, SchemaInfo, TriggerInfo,
    TxStep, UniqueKeyInfo,
};
use duckdb::{Connection, InterruptHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// The path that means "no file at all".
///
/// Its own case rather than a path that happens not to exist: an in-memory
/// database is a first-class DuckDB session — a scratch analysis with data
/// attached or imported into it — and the registry hands this driver whatever
/// follows `duckdb://`, so it arrives spelled exactly like this.
const IN_MEMORY: &str = ":memory:";

#[derive(Debug, thiserror::Error)]
pub enum DuckError {
    #[error("{0}")]
    DuckDB(#[from] duckdb::Error),
    /// The same failure, carrying the statement it came from.
    ///
    /// A variant of its own because DuckDB gives out the position it found by
    /// printing a caret under the statement and nothing else — the C API has no
    /// accessor for one — so reading that caret needs the text that was sent,
    /// and `duckdb::Error` does not keep it. `rusqlite::Error::SqlInputError`
    /// does, which is why `driver-sqlite` needs nothing like this.
    #[error("{source}")]
    InStatement { sql: String, source: duckdb::Error },
    #[error("no database file at {0}")]
    NoSuchDatabase(String),
    /// Raised before a row moves, by `duckdb-rs` rather than by DuckDB: the
    /// binding refuses any result containing a `VARIANT`. Restated here because
    /// its own message names a Rust concept and does not say what to do instead.
    #[error(
        "column {column} of this result is a {duckdb_type} value, which this client cannot decode; \
         select it as CAST(<column> AS VARCHAR) to see its text"
    )]
    Undecodable { column: usize, duckdb_type: String },
    /// A column that has to be rendered to text — see `arrow_map` — and that
    /// arrow-rs will not render.
    #[error(
        "column {column:?} holds {arrow_type}, which this client cannot render; \
         select it as CAST({column} AS VARCHAR) to see its text"
    )]
    Unreadable { column: String, arrow_type: String },
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    /// Asked for a savepoint on a database that has none.
    ///
    /// Its own variant rather than the parser error DuckDB would raise, because
    /// the two are different news. "syntax error at or near SAVEPOINT" reads as
    /// this client having sent something malformed; what happened is that the
    /// database does not have the feature, and the transaction the caller is in
    /// is still open and still fine.
    #[error("DuckDB has no savepoints: the transaction can be committed or rolled back as a whole")]
    NoSavepoints,
    #[error("the thread this statement was running on stopped before it said why")]
    ReaderGone,
}

impl DuckError {
    /// Where in the statement DuckDB says the trouble is: a 1-based index,
    /// counted in characters, into the SQL that was sent.
    ///
    /// Scraped, and knowingly so. Both other drivers read a number off a
    /// structured error — `rusqlite::Error::SqlInputError`'s offset,
    /// `tokio_postgres`'s `ErrorPosition::Original` — and `duckdb::Error` has
    /// no field for one. What DuckDB does have is the caret block it prints
    /// under the statement, which is the database's own answer rather than this
    /// side searching the message for a word it recognises; see
    /// `caret_position` for what it takes to believe it.
    pub fn statement_position(&self) -> Option<u32> {
        let DuckError::InStatement {
            sql,
            source: duckdb::Error::DuckDBFailure(_, Some(message)),
        } = self
        else {
            return None;
        };
        caret_position(sql, message)
    }

    /// Attaches the statement a failure came from.
    ///
    /// Applied where a statement the caller wrote is run, and nowhere else: a
    /// catalog query failing is this driver's problem, and putting a caret into
    /// SQL the user never saw would point at nothing they can act on.
    fn in_statement(self, sql: &str) -> Self {
        match self {
            DuckError::DuckDB(source) => DuckError::InStatement {
                sql: sql.to_string(),
                source,
            },
            other => other,
        }
    }

    fn native(&self) -> Option<&duckdb::Error> {
        match self {
            DuckError::DuckDB(e) | DuckError::InStatement { source: e, .. } => Some(e),
            _ => None,
        }
    }

    /// Whether this statement stopped because somebody asked it to.
    ///
    /// Read from what DuckDB reported rather than from this side remembering
    /// that it called `cancel`, for the reason both other drivers give: a
    /// statement can fail on its own merits in the same moment the interrupt
    /// lands, and reporting that as "Cancelled" hides a real fault behind a
    /// button.
    ///
    /// Weaker than their versions and that is the finding. PostgreSQL answers
    /// this from `SqlState::QUERY_CANCELED` and SQLite from
    /// `ErrorCode::OperationInterrupted`; DuckDB carries a structured error type
    /// on some paths but not on this one — a fetch that is interrupted reaches
    /// `duckdb_failure_from_message`, which keeps the text and drops the code.
    /// So this matches a prefix, and a test interrupts a real query to pin it,
    /// so that a change to the wording fails the suite instead of quietly
    /// turning every cancellation into an error banner.
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self.native(),
            Some(duckdb::Error::DuckDBFailure(_, Some(message)))
                if message.starts_with(INTERRUPTED)
        )
    }
}

/// What DuckDB 1.5.5 says when a statement is interrupted: the whole message is
/// `INTERRUPT Error: Interrupted!`. Matched on the prefix, so a version that
/// says more after the colon still reads as a cancellation.
const INTERRUPTED: &str = "INTERRUPT Error:";

/// A failure from the caller's own statement, told which statement it was.
///
/// One case is not DuckDB's failure at all and must not be reported as one:
/// `duckdb-rs` checks a result's logical types before execution and refuses the
/// ones it cannot import — `VARIANT` today — with a message naming a Rust
/// concept and no advice. It happens before a row moves, so there is no schema
/// to take a column name from and the index is all there is to say.
fn statement_failed(e: duckdb::Error, sql: &str) -> DuckError {
    match e {
        duckdb::Error::FromSqlConversionFailure(index, duckdb_type, _) => DuckError::Undecodable {
            // One-based, as `ColumnInfo::position` is, so that the number in the
            // message counts the way the structure pane counts.
            column: index + 1,
            duckdb_type: duckdb_type.to_string(),
        },
        other => DuckError::DuckDB(other).in_statement(sql),
    }
}

/// The 1-based character offset the caret in a DuckDB error message points at.
///
/// DuckDB prints a fault as three parts — a sentence, the offending line as
/// `LINE n: <text>`, and a line of spaces ending in `^`. The caret is the only
/// position DuckDB gives out; there is no accessor for one in the C API.
///
/// Believing it takes one correction. The padding DuckDB writes counts *only
/// single-byte characters*: `SELECT 1 FROM é WHERE ORDER BY x` puts the caret 21
/// columns in, where the ORDER it is pointing at is the 23rd character and the
/// 24th byte. So this inverts that rule rather than trusting the column, and the
/// difference is invisible until somebody names a table in a language that is
/// not English — which is exactly why the test uses one.
///
/// Everything that does not fit answers `None`. A caret this side could not
/// place is worth less than no caret at all: the front end draws one either way,
/// and a wrong one sends the reader to the wrong part of their statement.
fn caret_position(sql: &str, message: &str) -> Option<u32> {
    let mut lines = message.lines().peekable();
    let (line_number, quoted, caret, prefix) = loop {
        let line = lines.next()?;
        let Some(rest) = line.strip_prefix("LINE ") else {
            continue;
        };
        let (number, quoted) = rest.split_once(": ")?;
        let Some(caret) = lines.peek() else { continue };
        // The padding includes whatever DuckDB printed before the statement, and
        // `LINE 12: ` is nine characters where `LINE 1: ` is eight.
        let prefix = "LINE ".len() + number.len() + ": ".len();
        break (number.parse::<usize>().ok()?, quoted, *caret, prefix);
    };

    let spaces = caret.len().checked_sub(1)?;
    if !caret.ends_with('^') || caret[..spaces].bytes().any(|b| b != b' ') {
        return None;
    }
    let column = spaces.checked_sub(prefix)?;

    // Counted over this side's copy of the statement, because the offset has to
    // be into what was sent. The quoted line is compared rather than used, so
    // that a message DuckDB shortened or reflowed answers `None` instead of
    // answering confidently about a different string.
    let mut before = 0usize;
    let mut target = None;
    for (index, line) in sql.split_inclusive('\n').enumerate() {
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        if index + 1 == line_number {
            target = Some((before, line));
            break;
        }
        // The newline is a character of the statement too.
        before += line.chars().count() + 1;
    }
    let (before, line) = target?;
    if line != quoted {
        return None;
    }

    let mut ascii = 0usize;
    let mut offset = None;
    for (index, c) in line.chars().enumerate() {
        if ascii == column {
            offset = Some(index);
            break;
        }
        if c.len_utf8() == 1 {
            ascii += 1;
        }
    }
    let offset = offset.or_else(|| (ascii == column).then(|| line.chars().count()))?;
    u32::try_from(before + offset + 1).ok()
}

/// Every interrupt handle this session currently has work running on.
///
/// Simpler than SQLite's, which wraps its handle in an `Arc` of its own because
/// `rusqlite::InterruptHandle` is not clonable. `Connection::interrupt_handle`
/// hands back an `Arc<InterruptHandle>` that is already `Send + Sync`, so there
/// is one fewer layer here for the same shape.
type Cancels = Arc<Mutex<Vec<(u64, Arc<InterruptHandle>)>>>;

/// One DuckDB database, and the connections opened against it.
///
/// Holds a connection rather than a path, which is the difference from
/// `SqliteSource` that matters most. A second SQLite connection is
/// `open(&path)`; a second DuckDB connection is `try_clone()`, which opens
/// another connection on the same `DatabaseHandle`. Reopening the path would
/// give a file-backed database a second instance with its own buffer pool and
/// its own locks, and would give an in-memory one a *different, empty database* —
/// so `try_clone` is not an optimisation here, it is the only thing that is
/// correct.
///
/// It is a `Mutex` because `Connection` is `Send` and not `Sync`. No pool: a
/// connection is a handle on a database this process already has open, so a
/// caller that needs one takes one, and the property a pool was protecting — a
/// navigator that does not queue behind a result being read — holds by
/// construction.
///
/// One of those connections is kept rather than taken and given back, and it is
/// the one statements run on. A transaction belongs to a connection, so a
/// `BEGIN` on a connection that is dropped afterwards opens a transaction the
/// next statement will not be given and nobody can commit. Everything else —
/// catalog reads, cursors — still takes a connection of its own.
pub struct DuckSource {
    seed: Arc<Mutex<Connection>>,
    /// Locked for as long as a statement is being read. `Statement` borrows its
    /// connection and is neither `Send` nor `Sync`, so a result holds the
    /// session until it is finished or dropped, and the next statement waits.
    /// That is what "these two statements are in the same transaction" costs on
    /// a database that is a library rather than a server.
    session: Arc<Mutex<Connection>>,
    /// Taken at connect and kept, because by cancel time the session is inside
    /// the statement that is to be stopped and there is no borrowing it back.
    session_interrupt: Arc<InterruptHandle>,
    cancels: Cancels,
    next_id: AtomicU64,
}

/// Keeps one connection's interrupt handle reachable by `cancel` for exactly as
/// long as there is something on it to interrupt.
///
/// Scoped rather than permanent, as in `driver-sqlite` and for the same two
/// reasons. A connection opened for one piece of work is dropped after it, and a
/// handle whose connection has gone is not something to leave lying in a list —
/// interrupting one is harmless, since `InterruptHandle::clear` nulls the
/// pointer when the connection goes, but harmless is not a reason to keep it.
/// The session's connection is the opposite case and wants the same treatment:
/// it outlives every statement on it, and being registered only while one is in
/// flight is what keeps a cancel aimed at one statement off the next.
struct Registration {
    id: u64,
    cancels: Cancels,
}

impl Registration {
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

/// Opens the database named by `path`, refusing to invent one.
///
/// DuckDB creates a database file for a path that is not there, and has no
/// configuration that means "open the existing one, read-write, do not create" —
/// `AccessMode::ReadOnly` refuses a missing file but would also refuse every
/// write. So the check `driver-sqlite` performs to get a better message is the
/// mechanism here. What it is guarding against is the same: a client that
/// answers a mistyped path with an empty database looks like it connected, and
/// the user goes looking for their tables instead of for their typo.
fn open(path: &Path) -> Result<Connection, DuckError> {
    let conn = if path == Path::new(IN_MEMORY) {
        Connection::open_in_memory()?
    } else {
        if !path.is_file() {
            return Err(DuckError::NoSuchDatabase(path.display().to_string()));
        }
        Connection::open(path)?
    };
    pin(&conn)?;
    Ok(conn)
}

/// A second connection to the same database, with the same settings on it.
///
/// The settings are re-applied because `SET` without a scope is per-session for
/// these five, and a reader whose Arrow settings differ from the connection the
/// schema was read on would produce columns of a different type.
fn clone_connection(seed: &Arc<Mutex<Connection>>) -> Result<Connection, DuckError> {
    let conn = seed
        .lock()
        .map_err(|_| DuckError::ReaderGone)?
        .try_clone()?;
    pin(&conn)?;
    Ok(conn)
}

fn pin(conn: &Connection) -> Result<(), DuckError> {
    conn.execute_batch(arrow_map::PINNED_SETTINGS)?;
    Ok(())
}

/// Runs `work` on a thread that is allowed to block, and treats a panic in it as
/// one.
///
/// Every call into DuckDB blocks, so none of them may run on a runtime worker.
async fn blocking<T, F>(work: F) -> Result<T, DuckError>
where
    F: FnOnce() -> Result<T, DuckError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(_) => Err(DuckError::ReaderGone),
    }
}

/// The connection one result is read on.
///
/// Two answers, and which one a result gets is the whole of this driver's
/// transaction story. A statement takes the session, so that a `BEGIN` and the
/// statements after it are on one connection. A cursor takes a connection of its
/// own, because it is held open by whoever is looking at it and the session
/// cannot be lent out for that long.
enum Reader {
    Session(Arc<Mutex<Connection>>),
    Own(Connection),
}

impl Reader {
    /// Runs `work` on the connection, holding the session for the whole of it if
    /// that is the connection this result is on.
    fn with<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, DuckError>,
    ) -> Result<T, DuckError> {
        match self {
            Reader::Own(conn) => work(conn),
            // A poisoned lock means a reader panicked with the connection
            // borrowed. What DuckDB was left in the middle of is not something
            // to guess at, so the session is treated as gone.
            Reader::Session(session) => {
                let session = session.lock().map_err(|_| DuckError::ReaderGone)?;
                work(&session)
            }
        }
    }
}

impl DuckSource {
    /// Opens `path`, or an in-memory database for `:memory:` and for nothing at
    /// all.
    pub async fn connect(path: &str) -> Result<Self, DuckError> {
        // The registry hands over whatever followed the scheme, so `duckdb://`
        // with nothing after it arrives here as an empty string. DuckDB reads
        // that as in-memory itself; saying so here means the two spellings
        // cannot drift apart.
        let path = PathBuf::from(if path.is_empty() { IN_MEMORY } else { path });
        let seed = Arc::new(Mutex::new(blocking(move || open(&path)).await?));

        // Cloned from the seed rather than opened from the path, which is what
        // makes `:memory:` work at all: a second `open_in_memory` is a
        // different, empty database, and the session would then be running
        // statements against a database the navigator cannot see.
        let cloned = Arc::clone(&seed);
        let session = blocking(move || clone_connection(&cloned)).await?;
        let session_interrupt = session.interrupt_handle();

        Ok(Self {
            seed,
            session: Arc::new(Mutex::new(session)),
            session_interrupt,
            cancels: Arc::new(Mutex::new(Vec::new())),
            next_id: AtomicU64::new(0),
        })
    }

    /// Asks DuckDB to abandon whatever this session is currently running.
    ///
    /// Not async, and the difference from PostgreSQL is the one `driver-sqlite`
    /// records: there a cancel travels to the server on a connection of its own,
    /// so it is something to await; here it sets a flag the running statement
    /// checks between data chunks, in this process, and there is nothing to wait
    /// for.
    ///
    /// Cooperative at chunk granularity, so the chunk in flight finishes. At
    /// 2048 rows that is not something a person notices.
    ///
    /// The session connection is reached the same way everything else is, by
    /// being in the registry only while it has something running, and that is
    /// what makes it safe to interrupt a connection several statements share:
    /// DuckDB clears the flag when a query begins, so a cancel cannot spill onto
    /// the statement after the one it was aimed at. What it does reach is the
    /// transaction — DuckDB invalidates an open one when a statement inside it
    /// fails, and an interrupted statement is a failed statement, so a
    /// cancellation mid-transaction leaves a transaction that can only be rolled
    /// back. That is the database's rule rather than this driver's arrangement,
    /// and the front end learns of it the way it learns anything else about the
    /// transaction: by asking after the call that could have changed it.
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
    /// Registered for the length of the call so that `cancel` reaches it. Most
    /// of these answer in microseconds; `referenced_by` reads every constraint
    /// in the database, and on a catalog with thousands of them it is the one
    /// somebody waits for.
    async fn with_connection<T, F>(&self, work: F) -> Result<T, DuckError>
    where
        F: FnOnce(&Connection) -> Result<T, DuckError> + Send + 'static,
        T: Send + 'static,
    {
        let seed = Arc::clone(&self.seed);
        let cancels = Arc::clone(&self.cancels);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        blocking(move || {
            let conn = clone_connection(&seed)?;
            let _registered = Registration::hold(cancels, id, conn.interrupt_handle());
            work(&conn)
        })
        .await
    }

    /// The navigator root: every schema of every database this connection can
    /// see, named `database.schema`.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, DuckError> {
        self.with_connection(metadata::schemas).await
    }

    /// Tables and views within one schema.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, DuckError> {
        let schema = schema.to_string();
        self.with_connection(move |conn| metadata::relations(conn, &schema))
            .await
    }

    /// Column definitions for one relation.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, DuckError> {
        let (schema, relation) = (schema.to_string(), relation.to_string());
        self.with_connection(move |conn| metadata::columns(conn, &schema, &relation))
            .await
    }

    /// The statement a view is defined by; `None` for a relation that has none.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, DuckError> {
        let (schema, relation) = (schema.to_string(), relation.to_string());
        self.with_connection(move |conn| metadata::definition(conn, &schema, &relation))
            .await
    }

    /// Indexes on one relation, which in DuckDB means the explicit ones.
    pub async fn indexes(&self, schema: &str, relation: &str) -> Result<Vec<IndexInfo>, DuckError> {
        let (schema, relation) = (schema.to_string(), relation.to_string());
        self.with_connection(move |conn| metadata::indexes(conn, &schema, &relation))
            .await
    }

    /// UNIQUE constraints on one relation, primary key excluded.
    pub async fn unique_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, DuckError> {
        let (schema, relation) = (schema.to_string(), relation.to_string());
        self.with_connection(move |conn| metadata::unique_keys(conn, &schema, &relation))
            .await
    }

    /// Foreign keys declared by one relation.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, DuckError> {
        let (schema, relation) = (schema.to_string(), relation.to_string());
        self.with_connection(move |conn| metadata::foreign_keys(conn, &schema, &relation))
            .await
    }

    /// Foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, DuckError> {
        let (schema, relation) = (schema.to_string(), relation.to_string());
        self.with_connection(move |conn| metadata::referenced_by(conn, &schema, &relation))
            .await
    }

    /// CHECK and UNIQUE constraints, with DuckDB's own rendering of each.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, DuckError> {
        let (schema, relation) = (schema.to_string(), relation.to_string());
        self.with_connection(move |conn| metadata::constraints(conn, &schema, &relation))
            .await
    }

    /// Always empty: DuckDB has no triggers. See `metadata`.
    pub async fn triggers(
        &self,
        _schema: &str,
        _relation: &str,
    ) -> Result<Vec<TriggerInfo>, DuckError> {
        Ok(Vec::new())
    }

    /// Takes one step of transaction control on the session connection.
    ///
    /// On the session and not on a connection cloned for it, which is the whole
    /// reason this driver holds one: a `BEGIN` on a connection that is dropped
    /// afterwards opens a transaction nothing can be added to and nobody can
    /// commit.
    ///
    /// Three of the six are refused, and refused rather than skipped, because
    /// DuckDB has no savepoints — `SAVEPOINT`, `ROLLBACK TO` and `RELEASE` are
    /// all syntax errors in its parser, not features behind a setting. A client
    /// that quietly did nothing would leave somebody believing there is a point
    /// they can come back to, and find out otherwise by rolling back further
    /// than they meant to.
    ///
    /// Waits for the session, which is not a formality here. A result being read
    /// has the connection until it is finished or dropped, so a Commit pressed
    /// while a grid is still filling lands after the last row rather than
    /// between two of them — and it lands, rather than being refused or applied
    /// to a different connection.
    pub async fn transaction(&self, step: &TxStep) -> Result<(), DuckError> {
        let statement = match step {
            TxStep::Begin => "BEGIN TRANSACTION",
            TxStep::Commit => "COMMIT",
            TxStep::Rollback => "ROLLBACK",
            TxStep::Savepoint(_) | TxStep::RollbackTo(_) | TxStep::Release(_) => {
                return Err(DuckError::NoSavepoints);
            }
        };
        let session = Arc::clone(&self.session);
        blocking(move || {
            session
                .lock()
                .map_err(|_| DuckError::ReaderGone)?
                .execute_batch(statement)?;
            Ok(())
        })
        .await
    }

    /// Prepare `sql` and begin streaming its result as Arrow batches of at most
    /// `batch_rows` rows.
    ///
    /// Resolves as soon as the statement has executed and before a single row
    /// has been read, which is PostgreSQL's contract restored: DuckDB knows
    /// every column's type at execution, so `Statement::schema()` answers before
    /// the first `step()`. SQLite has to wait for the first row because a
    /// column's type there is not settled without seeing a value. So "schema
    /// known before rows" is not an embedded-versus-server split — it is a fact
    /// about each database.
    ///
    /// Runs on the session connection, so that a statement and the `BEGIN`
    /// before it are the same connection's business. The consequence is that a
    /// result nobody has finished reading is a session nobody else can use: the
    /// next statement waits for this one to reach its last chunk or be dropped.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, DuckError> {
        self.stream(
            Reader::Session(Arc::clone(&self.session)),
            Arc::clone(&self.session_interrupt),
            sql,
            batch_rows,
        )
        .await
    }

    /// Open a cursor over `sql` and return a handle to fetch pages.
    ///
    /// The same thing as `query` apart from the connection, as in
    /// `driver-sqlite`, and reached by a different route worth writing down.
    /// What Phase 1 wanted from a cursor was that page two agrees with page one;
    /// PostgreSQL needs `DECLARE CURSOR` because otherwise each page is a
    /// statement with a snapshot of its own, and SQLite gets it from holding a
    /// read transaction across a statement's steps. DuckDB is MVCC: a statement
    /// reads the snapshot fixed when it began, so a streaming result read
    /// forward is already one consistent view.
    ///
    /// On a connection of its own, and that part is not incidental. A cursor is
    /// handed to the caller to hold, and the session cannot be lent out for as
    /// long as somebody leaves a table browser open — the trait says the same
    /// thing from the other side, that a cursor is outside whatever transaction
    /// the session has open.
    ///
    /// Deliberately no `BEGIN` around it, following SQLite's driver: that would
    /// hold a transaction open for as long as somebody leaves a table browser
    /// open, blocking checkpointing and growing the write-ahead log, in exchange
    /// for a guarantee already in hand.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, DuckError> {
        let seed = Arc::clone(&self.seed);
        let conn = blocking(move || clone_connection(&seed)).await?;
        // Taken before the statement runs, for the reason `Cursor::canceller`
        // gives: by cancel time the connection is inside the fetch that is to be
        // stopped.
        let interrupt = conn.interrupt_handle();
        let stream = self
            .stream(Reader::Own(conn), interrupt, sql, batch_rows)
            .await?;
        Ok(Cursor { stream })
    }

    async fn stream(
        &self,
        reader: Reader,
        interrupt: Arc<InterruptHandle>,
        sql: &str,
        batch_rows: usize,
    ) -> Result<ArrowStream, DuckError> {
        // A zero-row batch would be emitted forever without moving the statement
        // forward, so the one value that cannot mean anything is read as the
        // smallest one that can.
        let batch_rows = batch_rows.max(1);
        let registration = self.register(Arc::clone(&interrupt));

        let (schema_tx, schema_rx) = oneshot::channel();
        // One batch in flight. The reader thread blocks on a full channel, which
        // stops it calling `step()`, which stops DuckDB producing — the bound
        // Phase 1 asked for, expressed as backpressure rather than as an
        // eviction policy.
        let (batch_tx, batches) = mpsc::channel(1);
        let rows_affected = Arc::new(AtomicI64::new(-1));

        let sql = sql.to_string();
        let affected = Arc::clone(&rows_affected);
        // A thread of its own rather than tokio's blocking pool. A result stays
        // open for as long as a front end holds it — a scroll position is not a
        // task that finishes — and a pool worker parked on one is a worker
        // nothing else can have. DuckDB makes it mandatory rather than merely
        // preferable: `Statement` is neither `Send` nor `Sync`, so the statement
        // and the result cannot leave the thread that made them.
        std::thread::spawn(move || {
            pump(reader, &sql, batch_rows, schema_tx, batch_tx, &affected);
        });

        let schema = schema_rx.await.map_err(|_| DuckError::ReaderGone)??;
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
    batches: mpsc::Receiver<Result<RecordBatch, DuckError>>,
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

    /// Rows produced, or `None` until the result has been read to the end.
    ///
    /// Rows produced and not rows changed, for every statement, which is the one
    /// place this driver answers something narrower than the other two. DuckDB
    /// reports a write's count as an ordinary result set — an `INSERT` returns
    /// one row with one `Count` column — and `Statement::row_count()` counts the
    /// rows of that result, so it is 1 for an `INSERT` of a thousand. The count
    /// the user wants is in the grid, and there is no reachable
    /// `duckdb_rows_changed` behind a streaming statement to read it from.
    ///
    /// Deliberately not guessed at from the shape of the result. A one-row,
    /// one-column `Count` is exactly what `SELECT count(*) AS "Count"` produces,
    /// and a number that is right for writes and silently wrong for that is
    /// worse than one that is always the same thing.
    pub fn rows_affected(&self) -> Option<u64> {
        u64::try_from(self.rows_affected.load(Ordering::Acquire)).ok()
    }

    /// Next batch, or `None` once the result is fully consumed.
    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, DuckError> {
        match self.batches.recv().await {
            Some(batch) => batch.map(Some),
            // The sender is gone, which is how the reader thread says it reached
            // the end. An error would have arrived through the channel first.
            None => Ok(None),
        }
    }
}

/// A result read a page at a time.
///
/// Holds a connection and a streaming statement for as long as it lives, so the
/// pages it hands out are pages of one result rather than of a database that
/// kept changing underneath.
pub struct Cursor {
    stream: ArrowStream,
}

/// Asks DuckDB to stop what a cursor is fetching.
///
/// Separate from the cursor so the two can be held at once: cancelling means
/// reaching the connection while a fetch has it, which is the whole situation.
pub struct CursorCancel {
    interrupt: Arc<InterruptHandle>,
}

impl CursorCancel {
    /// Raised is not interrupted: a fetch that had already finished leaves
    /// nothing to stop, and what actually happened shows up as the fetch failing
    /// with `is_cancelled`.
    pub fn cancel(&self) {
        self.interrupt.interrupt();
    }
}

impl Cursor {
    /// Fetch the next page, or `None` once the cursor has reached the end.
    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, DuckError> {
        self.stream.next_batch().await
    }

    pub fn schema(&self) -> SchemaRef {
        self.stream.schema()
    }

    pub fn canceller(&self) -> CursorCancel {
        CursorCancel {
            interrupt: Arc::clone(&self.stream.interrupt),
        }
    }

    /// Close the cursor explicitly.
    ///
    /// Optional: dropping it does the same. Closing the channel is what tells
    /// the reader thread to stop, and the thread ending is what closes the
    /// connection under the statement.
    pub async fn close(&mut self) -> Result<(), DuckError> {
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
/// is held here: the connection or a lock on it, the statement prepared from
/// that, and the chunks stepped out of the statement. None of those is `Send`,
/// which is why this is a thread and not a task.
///
/// The connection is given up before the last batch is, which is what lets a
/// caller run the next statement the moment it has read this one to the end: the
/// session lock goes when `with` returns and `batch_tx` goes when this does, so
/// the `None` that says "finished" cannot arrive while the session is still
/// held.
fn pump(
    reader: Reader,
    sql: &str,
    batch_rows: usize,
    schema_tx: oneshot::Sender<Result<SchemaRef, DuckError>>,
    batch_tx: mpsc::Sender<Result<RecordBatch, DuckError>>,
    rows_affected: &AtomicI64,
) {
    let mut schema_tx = Some(schema_tx);
    let result = reader.with(|conn| {
        read(
            conn,
            sql,
            batch_rows,
            &mut schema_tx,
            &batch_tx,
            rows_affected,
        )
    });
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
    schema_tx: &mut Option<oneshot::Sender<Result<SchemaRef, DuckError>>>,
    batch_tx: &mpsc::Sender<Result<RecordBatch, DuckError>>,
    rows_affected: &AtomicI64,
) -> Result<(), DuckError> {
    let mut stmt = conn.prepare(sql).map_err(|e| statement_failed(e, sql))?;
    // Called for its effect on the statement — `duckdb_execute_prepared_streaming`
    // rather than the materialising `duckdb_execute_prepared` — and then dropped.
    // Its `Iterator` is the panicking path this whole file exists to avoid; the
    // loop below reads `step()` off the same statement, which returns a `Result`.
    drop(
        stmt.stream_arrow([])
            .map_err(|e| statement_failed(e, sql))?,
    );

    let layout = Layout::of(&stmt.schema())?;
    let schema = layout.schema();
    let sent = schema_tx
        .take()
        .expect("the schema is sent exactly once, here")
        .send(Ok(Arc::clone(&schema)));
    if sent.is_err() {
        // Nobody is waiting for this result any more. Not an error: it is what a
        // front end dropping a query before its first batch looks like.
        return Ok(());
    }

    let mut produced: i64 = 0;
    while let Some(chunk) = stmt.step().map_err(|e| statement_failed(e, sql))? {
        let batch = layout.apply(RecordBatch::from(&chunk))?;
        produced += batch.num_rows() as i64;
        // Sliced rather than concatenated. A DuckDB chunk is at most 2048 rows,
        // so `batch_rows` can only ever be honoured downward — and slicing
        // shares the buffers, where growing a batch to the asked-for size would
        // copy every one of them.
        for start in (0..batch.num_rows()).step_by(batch_rows) {
            let rows = batch_rows.min(batch.num_rows() - start);
            if batch_tx
                .blocking_send(Ok(batch.slice(start, rows)))
                .is_err()
            {
                return Ok(());
            }
        }
    }

    rows_affected.store(produced, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message DuckDB 1.5.5 prints under a statement it could not parse.
    fn parser_error(line: &str) -> String {
        format!("Parser Error: syntax error at or near \"ORDER\"\n\nLINE 1: {line}\n")
    }

    #[test]
    fn a_caret_under_an_ascii_statement_points_where_it_looks() {
        let sql = "SELECT id FROM nums WHERE ORDER BY id";
        let message = parser_error(sql) + "                                  ^";
        // "ORDER" is the 27th character, counting from one.
        assert_eq!(caret_position(sql, &message), Some(27));
        assert_eq!(&sql[26..31], "ORDER");
    }

    #[test]
    fn a_caret_is_corrected_for_the_characters_duckdb_did_not_count() {
        // DuckDB's padding counts single-byte characters only, so the caret it
        // writes under a statement with an accented identifier sits four columns
        // early. Taking the column at face value would send the reader into the
        // middle of the table name.
        let sql = "SELECT 1 FROM ünïcödé WHERE ORDER BY x";
        let message = parser_error(sql) + "                                ^";
        let position = caret_position(sql, &message).expect("a position");
        assert_eq!(position, 29);
        let start = sql
            .char_indices()
            .nth(position as usize - 1)
            .expect("inside the statement")
            .0;
        assert_eq!(&sql[start..start + 5], "ORDER");
    }

    #[test]
    fn a_caret_on_a_later_line_counts_the_lines_before_it() {
        let sql = "SELECT 1\nFROM nums\nWHERE ORDER BY id";
        let message = "Parser Error: syntax error at or near \"ORDER\"\n\nLINE 3: WHERE ORDER BY id\n              ^";
        let position = caret_position(sql, message).expect("a position");
        assert_eq!(position, 26);
        assert!(sql[position as usize - 1..].starts_with("ORDER"));
    }

    #[test]
    fn a_message_that_quotes_a_different_statement_is_not_believed() {
        // The guard that keeps a shortened or reflowed line from being measured
        // against the statement this side holds.
        let sql = "SELECT id FROM nums WHERE ORDER BY id";
        let message = parser_error("SELECT id FROM nums WHERE ORD...") + "        ^";
        assert_eq!(caret_position(sql, &message), None);
    }

    #[test]
    fn a_message_with_no_caret_in_it_says_nothing() {
        assert_eq!(caret_position("SELECT 1", "Out of Memory Error: ..."), None);
    }

    /// Needs no database — it needs the absence of one, which is why it can run
    /// in the unit suite.
    #[tokio::test]
    async fn a_path_with_no_database_behind_it_says_which_path() {
        let err = DuckSource::connect("/nonexistent/directory/warehouse.duckdb")
            .await
            .err()
            .expect("nothing is at that path");
        let message = err.to_string();
        assert!(
            message.contains("warehouse.duckdb"),
            "the message should name the path that failed, got: {message}"
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_not_quietly_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typo.duckdb");
        assert!(DuckSource::connect(path.to_str().unwrap()).await.is_err());
        // DuckDB would have created it. A client that answers a mistyped path
        // with an empty database looks like it connected, and the user goes
        // looking for their tables instead of for their typo.
        assert!(!path.exists(), "connecting must not create the database");
    }
}
