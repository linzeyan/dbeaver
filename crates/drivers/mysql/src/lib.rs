//! A MySQL read path: connect, execute, stream Arrow record batches, page a
//! cursor, and stop a statement that is running.
//!
//! Also the read path for the databases that speak MySQL's protocol. Nothing
//! here branches on a product name or a version string — TiDB reports a MySQL
//! version that its own configuration can override, so a version test is broken
//! by construction. Where a feature has to be detected it is detected by asking
//! `information_schema` what tables and columns it has, which is a question
//! every one of these servers answers about itself truthfully because that is
//! how they announce compatibility.
//!
//! Three shapes here differ from the PostgreSQL driver, and all three come from
//! one property of the client protocol: a MySQL connection carries one command
//! at a time, and `mysql_async` enforces that with a `&mut Conn` borrow that
//! lasts as long as the result being read.
//!
//! **A result owns a connection.** `query` and `cursor` are the same mechanism
//! here — one `exec_iter` read forward by a task that owns the connection
//! outright — because there is no other way to hold a result and answer
//! anything else at the same time. PostgreSQL can run statements on the session
//! connection because its stream does not borrow the client; MySQL cannot.
//!
//! **There is no server-side cursor worth having.** MySQL's only one
//! materializes the whole result into an internal temporary table and then
//! hands it back a row per round trip, which is the opposite of what a
//! million-row grid needs. Reading one long result forward gives the two
//! properties the trait actually asks for — page *n* costs what page one costs,
//! and the pages agree with each other — for a better reason than a cursor
//! would: there is only ever one execution, so there is nothing for a
//! concurrent write to skew between pages, and no order to page by, so a table
//! with no primary key works like any other.
//!
//! **Cancelling is a statement on another connection.** `KILL QUERY <id>`,
//! where the id is the one `CONNECTION_ID()` returns.

mod arrow_map;
mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use arrow_map::{ColBuilder, ColumnType};
use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationshipInfo, SchemaInfo, TriggerInfo,
};
use futures_util::StreamExt;
use metadata::Capabilities;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, Row};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// `ER_QUERY_INTERRUPTED`. What a statement stopped by `KILL QUERY` fails with,
/// verified against 8.4 by killing a scan mid-flight.
const ER_QUERY_INTERRUPTED: u16 = 1317;

/// MariaDB's `ER_CONNECTION_KILLED`. Absent from MySQL's error space — 8.4's
/// client-facing numbers skip 1927 entirely — so testing for it costs nothing
/// and covers a server this driver is expected to reach.
const ER_CONNECTION_KILLED: u16 = 1927;

/// `ER_NO_SUCH_THREAD`. The answer to killing a connection that has already
/// closed, which is an ordinary race rather than a failure.
const ER_NO_SUCH_THREAD: u16 = 1094;

/// Connections kept for the next metadata call rather than closed.
///
/// Small on purpose: a navigator expands one node at a time, and MySQL's
/// default `max_connections` is 151 for the whole server.
const IDLE_CONNECTIONS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum MySqlError {
    #[error("{}", describe(.0))]
    MySql(#[from] mysql_async::Error),
    #[error("column {column:?} has unsupported type {mysql_type}")]
    UnsupportedType { column: String, mysql_type: String },
    #[error("column {column:?} wanted {expected} and got {value}")]
    Decode {
        column: String,
        expected: &'static str,
        value: String,
    },
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
}

/// Renders a failure the way a banner should read.
///
/// A server error's own `Display` is `ERROR 1146 (42S02): Table 'x' doesn't
/// exist`; the number and the SQLSTATE are read off separately as facts, and
/// repeating them in the sentence leaves the reader to skip past two pieces of
/// punctuation to find out what happened. Everything else keeps its full text,
/// including the source chain, because a connection failure whose message stops
/// at "Input/output error" fits every reason there is.
fn describe(e: &mysql_async::Error) -> String {
    match e {
        mysql_async::Error::Server(server) => server.message.clone(),
        other => other.to_string(),
    }
}

impl MySqlError {
    /// Where in the statement the server says the trouble is — never, on this
    /// database.
    ///
    /// MySQL's parse error carries no offset. What it sends instead is the tail
    /// of the statement from the point it gave up: `... near 'ORDER BY id' at
    /// line 1`. A position could be reconstructed by searching the statement for
    /// that fragment, and it is left unreconstructed deliberately — the
    /// fragment is truncated at 80 characters, it is not escaped, and a repeated
    /// clause puts the caret on the first occurrence rather than the one that
    /// failed. A caret in the wrong place is worse than no caret, because the
    /// reader believes it.
    ///
    /// Kept as a method rather than dropped so that the boundary conversion
    /// looks the same in every driver and this answer is stated somewhere.
    pub fn statement_position(&self) -> Option<u32> {
        None
    }

    /// Whether the server stopped this statement because somebody asked it to.
    ///
    /// Read off the error code the server sent rather than off what this side
    /// remembers doing: a statement can fail on its own merits in the same
    /// moment a `KILL` lands, and only the server knows which happened.
    ///
    /// Three things this deliberately does not treat as a cancellation.
    /// `ER_QUERY_TIMEOUT` (3024) is `max_execution_time` expiring, which nobody
    /// pressed a button for and which wants a different banner. A dropped socket
    /// is not one either — `KILL CONNECTION`, which this driver never issues,
    /// arrives indistinguishably from a network fault, and labelling a real
    /// fault "cancelled" hides it behind a button. Nor is the SQLSTATE enough:
    /// `70100` covers the timeout as well.
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            MySqlError::MySql(mysql_async::Error::Server(e))
                if e.code == ER_QUERY_INTERRUPTED || e.code == ER_CONNECTION_KILLED
        )
    }

    /// Whether the connection this came from is still in step.
    ///
    /// A rejected statement leaves the socket usable; anything else does not.
    fn left_the_connection_usable(&self) -> bool {
        match self {
            MySqlError::MySql(e) => !e.is_fatal(),
            _ => true,
        }
    }
}

/// A connection's server-side id, unregistered when the connection goes away.
///
/// Registered rather than looked up at cancel time because `KILL` names a
/// connection and the caller cannot see which one is busy. Unregistered on drop
/// because MySQL reuses a connection id once its connection closes, and an id
/// left behind would eventually name a session that belongs to somebody else.
struct Registration {
    live: Arc<Mutex<Vec<u64>>>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = live.iter().position(|id| *id == self.id) {
            live.swap_remove(at);
        }
    }
}

/// One connection, and the number that names it to `KILL`.
struct Session {
    conn: Conn,
    live: Registration,
}

/// A MySQL session: connections opened as they are needed, and the ids to stop
/// them by.
///
/// There is no long-lived "session connection" as in the PostgreSQL driver,
/// because a MySQL connection reading a result cannot do anything else. What a
/// caller gets instead is that a metadata call never waits behind a result:
/// results take a connection of their own for as long as they last, metadata
/// borrows one from a small idle set and puts it back.
///
/// The consequence worth stating: statements do not share a transaction. Every
/// `query` runs on whichever connection was free, so `BEGIN` in one statement
/// and `COMMIT` in the next would not be the same transaction. Phase 2 has no
/// transaction control, and the day it does, a statement connection has to be
/// pinned to the session.
pub struct MySqlSource {
    opts: Opts,
    idle: Arc<Mutex<Vec<Session>>>,
    live: Arc<Mutex<Vec<u64>>>,
    caps: Capabilities,
}

impl MySqlSource {
    /// Opens a session against `url`, which is `mysql://user:pass@host:port/db`.
    ///
    /// The database in the path is optional and is only a default for
    /// unqualified names: MySQL's databases are siblings on one server rather
    /// than islands, every metadata call here names its schema as a bound
    /// parameter, and `schemas()` lists all of them whichever one is current.
    pub async fn connect(url: &str) -> Result<Self, MySqlError> {
        let opts = Opts::from_url(url).map_err(mysql_async::Error::Url)?;
        // Every connection this driver opens is UTC, which is what makes the
        // `UTC` tag on a `TIMESTAMP` column true. The server converts a
        // `TIMESTAMP` from storage into the session's zone before it reaches
        // the wire, so without this the tag would name a zone the values are
        // not in, and the alternative — reading the session zone and applying an
        // offset per value — is arithmetic on every timestamp cell that is
        // wrong the moment anyone issues `SET time_zone`. The cost is visible
        // and belongs on the record: `NOW()` in the query tab answers in UTC.
        let opts = Opts::from(OptsBuilder::from_opts(opts).init(vec!["SET time_zone = '+00:00'"]));

        let live: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        // Opened eagerly so a wrong password fails here rather than at the first
        // expanded node.
        let mut first = open(&opts, &live).await?;
        let caps = metadata::probe(&mut first.conn).await?;

        Ok(Self {
            opts,
            idle: Arc::new(Mutex::new(vec![first])),
            live,
            caps,
        })
    }

    /// A connection to run one metadata call on.
    async fn acquire(&self) -> Result<Session, MySqlError> {
        let pooled = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        match pooled {
            Some(session) => Ok(session),
            None => open(&self.opts, &self.live).await,
        }
    }

    /// Puts a connection back, unless the failure it just had left it unusable.
    ///
    /// A connection broken by an I/O fault and returned to the idle set would
    /// fail every call after the one that broke it, and the pool would hand it
    /// out again each time.
    fn release<T>(&self, session: Session, outcome: &Result<T, MySqlError>) {
        if let Err(e) = outcome
            && !e.left_the_connection_usable()
        {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < IDLE_CONNECTIONS {
            idle.push(session);
        }
    }

    /// Asks the server to abandon whatever this session is running.
    ///
    /// The request is an ordinary statement on a connection of its own, because
    /// the protocol carries one command per connection: sent in-band it would
    /// queue behind the statement it exists to interrupt.
    ///
    /// Every connection this session has open is named, not just a busy one.
    /// The caller cannot see which is busy, and a `KILL QUERY` against an idle
    /// connection is a documented no-op that costs a round trip — which is the
    /// price of not having to know. A connection that closed between reading the
    /// registry and sending the statement answers `ER_NO_SUCH_THREAD`, and that
    /// is the same no-op arriving a moment later rather than a failure.
    ///
    /// Best-effort, as in every driver here: success means the requests were
    /// delivered. What actually happened surfaces where the statement is, as a
    /// failure whose `is_cancelled` is true, or as no failure at all.
    pub async fn cancel(&self) -> Result<(), MySqlError> {
        let ids = self.live.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if ids.is_empty() {
            return Ok(());
        }
        // Deliberately not registered: a connection that killed itself would be
        // an interesting way to lose the cancel.
        let mut killer = Conn::new(self.opts.clone()).await?;
        for id in ids {
            // Formatted rather than bound. The id is a `u64` this side read off
            // the server, so it cannot be an injection, and `KILL` does not have
            // to be a preparable statement for this to work.
            match killer.query_drop(format!("KILL QUERY {id}")).await {
                Ok(()) => {}
                Err(mysql_async::Error::Server(e)) if e.code == ER_NO_SUCH_THREAD => {}
                Err(e) => return Err(e.into()),
            }
        }
        killer.disconnect().await?;
        Ok(())
    }

    /// Runs `sql` and streams its result in batches of at most `batch_rows`.
    ///
    /// Resolves once the statement is prepared, which is where the column types
    /// become known and therefore where a grid can be laid out. A statement that
    /// fails to parse fails here; one that fails while running fails from
    /// `next_batch`.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, MySqlError> {
        let reader = self.read(sql, batch_rows).await?;
        Ok(ArrowStream { reader })
    }

    /// Reads `sql` forward, a page at a time.
    ///
    /// The same mechanism `query` uses. On this database the two differ only in
    /// what the caller is handed: a cursor is meant to be held, so it carries a
    /// canceller of its own — `MySqlSource::cancel` cannot do that job, because
    /// the cursor's connection is its own and the cursor outlives the call that
    /// made it.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, MySqlError> {
        let reader = self.read(sql, batch_rows).await?;
        Ok(Cursor {
            canceller: CursorCancel {
                opts: self.opts.clone(),
                id: reader.id,
            },
            reader,
        })
    }

    /// Opens a connection, hands it to a task, and waits for the columns.
    ///
    /// A connection of its own rather than one from the idle set: a result holds
    /// its connection for as long as it is being read, and a caller that leaves
    /// a grid open would otherwise be holding a quarter of the pool.
    async fn read(&self, sql: &str, batch_rows: usize) -> Result<Reader, MySqlError> {
        let session = open(&self.opts, &self.live).await?;
        let id = session.live.id;
        let (schema_tx, schema_rx) = oneshot::channel();
        // Capacity one, so a reader that stops reading stops the producer
        // rather than letting it run ahead into memory. The bound on what a
        // result costs is this number times the batch size.
        let (pages_tx, pages_rx) = mpsc::channel(1);
        tokio::spawn(pump(
            session,
            sql.to_string(),
            batch_rows,
            schema_tx,
            pages_tx,
        ));

        let schema = match schema_rx.await {
            Ok(result) => result?,
            // The task cannot end before answering unless it panicked, and a
            // panic there has already been reported by the runtime.
            Err(_) => {
                return Err(MySqlError::Decode {
                    column: sql.to_string(),
                    expected: "a prepared statement",
                    value: "a reader that stopped before describing its columns".to_string(),
                });
            }
        };

        Ok(Reader {
            schema,
            pages: pages_rx,
            rows_affected: None,
            id,
        })
    }

    /// Databases on this server, which is the level MySQL calls both a schema
    /// and a database and the level the navigator hangs relations off.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::schemas(&mut session.conn).await;
        self.release(session, &out);
        out
    }

    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::relations(&mut session.conn, schema).await;
        self.release(session, &out);
        out
    }

    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::columns(&mut session.conn, schema, relation).await;
        self.release(session, &out);
        out
    }

    /// The body of a view; `None` for anything that is not one.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::definition(&mut session.conn, schema, relation).await;
        self.release(session, &out);
        out
    }

    pub async fn indexes(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<IndexInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::indexes(&mut session.conn, schema, relation, &self.caps).await;
        self.release(session, &out);
        out
    }

    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::foreign_keys(&mut session.conn, schema, relation).await;
        self.release(session, &out);
        out
    }

    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::referenced_by(&mut session.conn, schema, relation).await;
        self.release(session, &out);
        out
    }

    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::constraints(&mut session.conn, schema, relation, &self.caps).await;
        self.release(session, &out);
        out
    }

    pub async fn triggers(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<TriggerInfo>, MySqlError> {
        let mut session = self.acquire().await?;
        let out = metadata::triggers(&mut session.conn, schema, relation).await;
        self.release(session, &out);
        out
    }
}

/// Opens a connection and registers the number that stops it.
///
/// The id comes from `SELECT CONNECTION_ID()` and not from the handshake, which
/// `mysql_async` exposes for free as a `u32`. One round trip buys correctness on
/// a large TiDB cluster, where connection ids widen to 64 bits and a truncated
/// one names a different session — killing somebody else's statement, or
/// nothing.
async fn open(opts: &Opts, live: &Arc<Mutex<Vec<u64>>>) -> Result<Session, MySqlError> {
    let mut conn = Conn::new(opts.clone()).await?;
    let id: u64 = conn
        .query_first("SELECT CONNECTION_ID()")
        .await?
        .unwrap_or_else(|| u64::from(conn.id()));
    live.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    Ok(Session {
        conn,
        live: Registration {
            live: Arc::clone(live),
            id,
        },
    })
}

/// One page on its way from the connection that produced it.
enum Page {
    Batch(RecordBatch),
    /// The last message, carrying the count that only exists once the result has
    /// been read to the end.
    Done(u64),
    Failed(MySqlError),
}

/// Reads one result forward on a connection nothing else can touch.
///
/// A task rather than a struct holding both the connection and the result:
/// `mysql_async`'s result borrows its connection, so the two cannot be handed
/// across an FFI boundary together without a self-referential type. Speaking
/// over channels removes that problem entirely and puts the memory bound in the
/// channel capacity, where it can be read.
async fn pump(
    session: Session,
    sql: String,
    batch_rows: usize,
    schema_out: oneshot::Sender<Result<SchemaRef, MySqlError>>,
    pages: mpsc::Sender<Page>,
) {
    // Destructured so the registration outlives the connection and is dropped
    // with this task, whatever way it ends.
    let Session { mut conn, live } = session;

    let prepared = match conn.prep(&*sql).await {
        Ok(prepared) => prepared,
        Err(e) => {
            let _ = schema_out.send(Err(e.into()));
            return;
        }
    };

    // Types are known here, before a row exists, because the server answers a
    // prepare with the column definitions. A statement that returns no result
    // set — an UPDATE, a CREATE — describes zero columns, which is the honest
    // schema for it.
    let specs: Vec<ColumnType> = prepared.columns().iter().map(ColumnType::of).collect();
    let fields = prepared
        .columns()
        .iter()
        .zip(&specs)
        .map(|(column, spec)| arrow_map::arrow_field(&column.name_str(), spec))
        .collect::<Result<Vec<_>, _>>();
    let schema = match fields {
        Ok(fields) => Arc::new(Schema::new(fields)),
        Err(e) => {
            let _ = schema_out.send(Err(e));
            return;
        }
    };
    let names: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    if schema_out.send(Ok(Arc::clone(&schema))).is_err() {
        return;
    }

    let mut produced = 0u64;
    let mut had_rows = false;
    let mut result = match conn.exec_iter(&prepared, ()).await {
        Ok(result) => result,
        Err(e) => {
            let _ = pages.send(Page::Failed(e.into())).await;
            return;
        }
    };

    {
        let rows = match result.stream::<Row>().await {
            Ok(rows) => rows,
            Err(e) => {
                let _ = pages.send(Page::Failed(e.into())).await;
                return;
            }
        };
        if let Some(mut rows) = rows {
            had_rows = true;
            loop {
                let mut builders: Vec<ColBuilder> = schema
                    .fields()
                    .iter()
                    .map(|f| ColBuilder::new(f, batch_rows))
                    .collect();
                let mut filled = 0usize;
                let mut failure = None;
                while filled < batch_rows {
                    match rows.next().await {
                        Some(Ok(row)) => {
                            let values = row.unwrap();
                            for ((builder, name), value) in
                                builders.iter_mut().zip(&names).zip(values)
                            {
                                if let Err(e) = builder.append(name, value) {
                                    failure = Some(e);
                                    break;
                                }
                            }
                            if failure.is_some() {
                                break;
                            }
                            filled += 1;
                        }
                        Some(Err(e)) => {
                            failure = Some(e.into());
                            break;
                        }
                        None => break,
                    }
                }

                if let Some(e) = failure {
                    let _ = pages.send(Page::Failed(e)).await;
                    return;
                }
                if filled == 0 {
                    break;
                }
                produced += filled as u64;
                let arrays = builders.iter_mut().map(|b| b.finish()).collect();
                match RecordBatch::try_new(Arc::clone(&schema), arrays) {
                    // A send that fails means the reader has gone; the
                    // connection closes with this task and the server sees the
                    // statement abandoned.
                    Ok(batch) => {
                        if pages.send(Page::Batch(batch)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = pages.send(Page::Failed(e.into())).await;
                        return;
                    }
                }
                if filled < batch_rows {
                    break;
                }
            }
        }
    }
    drop(result);

    // What the number counts depends on the statement, which is what the trait
    // says: rows produced for one that reads, rows changed for one that writes.
    let affected = if had_rows {
        produced
    } else {
        conn.affected_rows()
    };
    let _ = pages.send(Page::Done(affected)).await;
    drop(conn);
    drop(live);
}

/// The reading half, shared by a result and a cursor because on this database
/// they are the same thing read at different speeds.
struct Reader {
    schema: SchemaRef,
    pages: mpsc::Receiver<Page>,
    rows_affected: Option<u64>,
    id: u64,
}

impl Reader {
    async fn next(&mut self) -> Result<Option<RecordBatch>, MySqlError> {
        match self.pages.recv().await {
            Some(Page::Batch(batch)) => Ok(Some(batch)),
            Some(Page::Done(affected)) => {
                self.rows_affected = Some(affected);
                Ok(None)
            }
            Some(Page::Failed(e)) => Err(e),
            // The producer ended without a word, which is what a closed reader
            // looks like from here.
            None => Ok(None),
        }
    }

    /// Stops the producer and drains what it had already handed over.
    ///
    /// Closing without draining would leave the producer parked on a send
    /// nobody will take, and the connection open behind it.
    async fn close(&mut self) {
        self.pages.close();
        while self.pages.recv().await.is_some() {}
    }
}

pub struct ArrowStream {
    reader: Reader,
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.reader.schema)
    }

    /// Rows the statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Zero is a real answer — an `UPDATE` that matched nothing — so "not known
    /// yet" cannot be zero, and MySQL's count does not exist until the OK packet
    /// that terminates the result has arrived.
    pub fn rows_affected(&self) -> Option<u64> {
        self.reader.rows_affected
    }

    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, MySqlError> {
        self.reader.next().await
    }
}

/// One result being read a page at a time, on a connection of its own.
pub struct Cursor {
    reader: Reader,
    canceller: CursorCancel,
}

impl Cursor {
    /// The columns the pages arrive in, known at open time because the statement
    /// was prepared to build them.
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.reader.schema)
    }

    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, MySqlError> {
        self.reader.next().await
    }

    /// A handle for stopping this cursor's fetch from another thread.
    ///
    /// Taken out in advance because by cancel time the cursor is borrowed by the
    /// fetch that is to be stopped, which is the whole situation.
    pub fn canceller(&self) -> CursorCancel {
        self.canceller.clone()
    }

    /// Closes the cursor and lets its connection go.
    ///
    /// Optional — dropping does the same — and safe to call with pages left,
    /// which is the ordinary case for a table browser somebody closed.
    pub async fn close(&mut self) -> Result<(), MySqlError> {
        self.reader.close().await;
        Ok(())
    }
}

/// Stops the fetch one cursor is running.
#[derive(Clone)]
pub struct CursorCancel {
    opts: Opts,
    id: u64,
}

impl CursorCancel {
    /// Delivered is not interrupted. A fetch that had already finished leaves
    /// nothing to stop and this still succeeds, as does cancelling a cursor
    /// whose connection has already closed.
    pub async fn cancel(&self) -> Result<(), MySqlError> {
        let mut killer = Conn::new(self.opts.clone()).await?;
        let sent = killer.query_drop(format!("KILL QUERY {}", self.id)).await;
        killer.disconnect().await?;
        match sent {
            Ok(()) => Ok(()),
            Err(mysql_async::Error::Server(e)) if e.code == ER_NO_SUCH_THREAD => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no database — it needs the absence of one, which is why it belongs
    /// in the unit suite. Port 1 is reserved and nothing listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let err = MySqlSource::connect("mysql://nobody@127.0.0.1:1/nothing")
            .await
            .err()
            .expect("nothing is listening on port 1");
        let message = err.to_string();
        assert!(
            message.to_lowercase().contains("refused"),
            "the refusal should survive into the message, got: {message}"
        );
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_fails_before_any_socket_is_opened() {
        let err = MySqlSource::connect("not a url at all")
            .await
            .err()
            .expect("this is not a connection string");
        assert!(!err.to_string().is_empty());
        assert!(!err.is_cancelled());
    }

    /// A cancellation is a fact about the failure, not a phrase in it.
    #[test]
    fn only_the_interrupt_codes_read_as_a_cancellation() {
        let interrupted = MySqlError::MySql(mysql_async::Error::Server(mysql_async::ServerError {
            code: ER_QUERY_INTERRUPTED,
            message: "Query execution was interrupted".to_string(),
            state: "70100".to_string(),
        }));
        assert!(interrupted.is_cancelled());

        // `max_execution_time` expiry shares the SQLSTATE and is not something
        // anybody pressed a button for, so a driver that matched on `70100`
        // would put a cancel banner on a timeout.
        let timed_out = MySqlError::MySql(mysql_async::Error::Server(mysql_async::ServerError {
            code: 3024,
            message: "Query execution was interrupted, maximum statement execution time exceeded"
                .to_string(),
            state: "70100".to_string(),
        }));
        assert!(!timed_out.is_cancelled());

        let syntax = MySqlError::MySql(mysql_async::Error::Server(mysql_async::ServerError {
            code: 1064,
            message: "You have an error in your SQL syntax".to_string(),
            state: "42000".to_string(),
        }));
        assert!(!syntax.is_cancelled());
    }

    #[test]
    fn a_failure_says_what_the_server_said_and_not_its_number() {
        // The code and the SQLSTATE are carried as facts elsewhere; repeating
        // them in the banner buries the sentence that says what happened.
        let e = MySqlError::MySql(mysql_async::Error::Server(mysql_async::ServerError {
            code: 1146,
            message: "Table 'bench.nope' doesn't exist".to_string(),
            state: "42S02".to_string(),
        }));
        assert_eq!(e.to_string(), "Table 'bench.nope' doesn't exist");
    }
}
