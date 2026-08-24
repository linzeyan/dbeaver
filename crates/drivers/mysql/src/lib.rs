//! A MySQL read path: connect, execute, stream Arrow record batches, page a
//! cursor, and stop a statement that is running.
//!
//! Also the read path for the databases that speak MySQL's protocol. Nothing
//! here branches on a product name or a version string — TiDB reports a MySQL
//! version that its own configuration can override, so a version test is broken
//! by construction. Where a feature has to be detected it is detected by asking
//! `information_schema` what tables and columns it has, which is a question
//! every one of these servers answers about itself truthfully because that is
//! how they announce compatibility — and where the catalog has nothing to say,
//! by asking the server to do the thing and seeing whether it will. Transaction
//! control is the second kind; see `metadata::probe`.
//!
//! Three shapes here differ from the PostgreSQL driver, and all three come from
//! one property of the client protocol: a MySQL connection carries one command
//! at a time, and `mysql_async` enforces that with a `&mut Conn` borrow that
//! lasts as long as the result being read.
//!
//! **A result holds its connection.** `query` and `cursor` are the same
//! mechanism here — one `exec_iter` read forward by a task that holds the
//! connection until the last page — because there is no other way to hold a
//! result and answer anything else at the same time. Statements still run on the
//! session connection, as they do in the PostgreSQL driver and for the same
//! reason, but here they take turns on it: the next statement waits for the
//! previous result to be read or dropped. That is what a transaction spanning
//! statements costs on this protocol, and there is nothing else to pay it with.
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
    ColumnInfo, ConstraintInfo, IndexInfo, InfoField, RelationInfo, RelationshipInfo, RoutineInfo,
    SchemaInfo, TriggerInfo, TxStep, UniqueKeyInfo,
};
use futures_util::StreamExt;
use metadata::Capabilities;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, Row};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};

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

/// A connection's server-side id, listed as one `cancel` should reach for as
/// long as this lives.
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

/// One spare connection, kept in the session's registry for as long as it is
/// held.
struct Pooled {
    conn: Conn,
    /// Held rather than read: it is what takes this connection's id back out of
    /// the registry when the connection is dropped instead of pooled.
    _live: Registration,
}

/// A MySQL session: one connection statements run on, spares for looking things
/// up, and the ids to stop any of them by.
///
/// Statements run on `session` and nothing else does, because a transaction
/// belongs to a connection: a `BEGIN` sent down a borrowed connection opens a
/// transaction the next statement will not be given and nobody can commit.
/// Metadata reads carry no transaction and answer quickly, so they borrow from
/// `idle` instead, and expanding a schema does not queue behind a result that is
/// still streaming.
///
/// What that arrangement costs, since it is not free here as it is in the
/// PostgreSQL driver: two statements cannot run at once. A MySQL connection
/// carries one command at a time and a result holds it until it has been read or
/// dropped, so a second `query` waits for the first result to finish — and a
/// result abandoned half way is drained off the socket before the connection can
/// take another command. Both are the protocol rather than this arrangement; the
/// alternative is statements that cannot share a transaction, which is what this
/// replaced.
pub struct MySqlSource {
    opts: Opts,
    /// The connection statements run on, and the only place a transaction on
    /// this session can live. Held for as long as a result is being read.
    session: Arc<tokio::sync::Mutex<Conn>>,
    /// The number that stops the session connection, put into the registry only
    /// while something is running on it. `cancel` says why not permanently.
    session_id: u64,
    idle: Arc<Mutex<Vec<Pooled>>>,
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
        //
        // The address in the URL is the address that gets connected to, which
        // is not the default: left alone, the client reads `@@socket` after the
        // handshake and, if that path is openable from here, drops the TCP
        // connection and reopens over the Unix socket instead. The server
        // reports the path it sees, so a forwarded port — a container, an SSH
        // tunnel — answers with a path belonging to its own filesystem, and
        // whatever happens to sit at that path on this machine is a different
        // server. A client whose job is to connect where it was told cannot
        // treat the port number as a hint.
        let opts = Opts::from(
            OptsBuilder::from_opts(opts)
                .init(vec!["SET time_zone = '+00:00'"])
                .prefer_socket(false),
        );

        let live: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        // Opened eagerly so a wrong password fails here rather than at the first
        // expanded node, and kept: this is the connection statements run on.
        let (mut session, session_id) = open(&opts).await?;
        let caps = metadata::probe(&mut session).await?;

        Ok(Self {
            opts,
            session: Arc::new(tokio::sync::Mutex::new(session)),
            session_id,
            // Empty. The session connection is already open, so a password that
            // is wrong has already been refused; the first metadata call opens
            // the first spare when it needs one.
            idle: Arc::new(Mutex::new(Vec::new())),
            live,
            caps,
        })
    }

    /// A connection to run one metadata call on, proven to still be open.
    ///
    /// A pooled connection is a cached thing the server may close without
    /// telling anybody, and MySQL guarantees it eventually does: `wait_timeout`
    /// ends an idle session after eight hours by default, a restart ends all of
    /// them at once, and an administrator's `KILL` ends one whenever they like.
    /// Handed out unchecked, the first call after any of those fails in the
    /// caller's face for something the caller did not do — and the call after it
    /// succeeds, which is the shape of a bug nobody can reproduce.
    ///
    /// One round trip is what finding out costs. The alternative is to run the
    /// call and repeat it on a new connection if the old one turned out to be
    /// gone, which is faster in the ordinary case and asks a worse question:
    /// whether the statement that just failed is safe to run twice. Asking the
    /// connection whether it is alive has one answer, and it is about the
    /// connection rather than about what was being run on it.
    async fn acquire(&self) -> Result<Pooled, MySqlError> {
        loop {
            let spare = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let Some(mut spare) = spare else { break };
            if spare.conn.ping().await.is_ok() {
                return Ok(spare);
            }
            // Dropped rather than put back, and the loop tries the next one:
            // whatever closed this connection — a restart, a `KILL` — has
            // probably closed the rest of the pool too.
        }
        let (conn, id) = open(&self.opts).await?;
        Ok(Pooled {
            conn,
            _live: register(&self.live, id),
        })
    }

    /// Puts a connection back, unless the failure it just had left it unusable.
    ///
    /// A connection broken by an I/O fault and returned to the idle set would
    /// fail every call after the one that broke it, and the pool would hand it
    /// out again each time.
    fn release<T>(&self, spare: Pooled, outcome: &Result<T, MySqlError>) {
        if let Err(e) = outcome
            && !e.left_the_connection_usable()
        {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < IDLE_CONNECTIONS {
            idle.push(spare);
        }
    }

    /// Asks the server to abandon whatever this session is running.
    ///
    /// The request is an ordinary statement on a connection of its own, because
    /// the protocol carries one command per connection: sent in-band it would
    /// queue behind the statement it exists to interrupt.
    ///
    /// Every spare connection the pool has opened is named, not just a busy one.
    /// The caller cannot see which is busy, and a `KILL QUERY` against an idle
    /// connection is a documented no-op that costs a round trip — which is the
    /// price of not having to know. A connection that closed between reading the
    /// registry and sending the statement answers `ER_NO_SUCH_THREAD`, and that
    /// is the same no-op arriving a moment later rather than a failure.
    ///
    /// The session's own connection is listed only while a statement or a
    /// transaction step is actually on it, and that asymmetry is the one thing
    /// here that is not free. `KILL QUERY` is a no-op on an idle connection only
    /// where the server makes it one: TiDB closes the connection instead, idle
    /// or busy — measured against 7.5 by watching `PROCESSLIST` — and a spare
    /// closed underneath this driver costs a reconnect nobody sees, while the
    /// session closed underneath it takes the open transaction with it. So that
    /// id goes into the registry when there is something to stop and comes out
    /// when there is not, which has the second effect that a Cancel pressed at a
    /// quiet moment opens no connection at all.
    ///
    /// What that does not fix, because nothing here can: on TiDB a Cancel
    /// pressed while a statement is running still ends the connection it was
    /// running on, transaction and all. The spelling that would not — `KILL TIDB
    /// QUERY` — is a product name written into a statement, which is the one
    /// thing this driver refuses to do.
    ///
    /// A cursor is the other exception, and it is an exception by construction
    /// rather than by care: its connection is never put in the registry, so a
    /// Cancel pressed over the editor cannot stop the table browser somebody
    /// left open beside it. Stopping a cursor is its canceller's job.
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

    /// Runs `sql` on the session connection and streams its result in batches of
    /// at most `batch_rows`.
    ///
    /// On the session and not on a spare, which is what makes a transaction
    /// worth having here: the `BEGIN` an earlier statement sent is still in
    /// force for this one. The price is that this waits for the previous result
    /// to be read or dropped, because the connection carries one command at a
    /// time.
    ///
    /// Resolves once the statement is prepared, which is where the column types
    /// become known and therefore where a grid can be laid out. A statement that
    /// fails to parse fails here; one that fails while running fails from
    /// `next_batch`.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, MySqlError> {
        let session = Arc::clone(&self.session).lock_owned().await;
        // Registered before the statement is sent and unregistered with the task
        // that reads it, so there is no moment where a statement is running on a
        // connection `cancel` does not know about — and none where an idle
        // connection is listed as though one were.
        let live = register(&self.live, self.session_id);
        let reader = read(
            Held::Session {
                conn: session,
                _live: live,
            },
            sql,
            batch_rows,
        )
        .await?;
        Ok(ArrowStream { reader })
    }

    /// Reads `sql` forward, a page at a time, on a connection of its own.
    ///
    /// The same mechanism `query` uses, on a different connection, and the two
    /// reasons are the same one seen from either end. A cursor is handed out to
    /// be held: on the session it would keep every other statement waiting for
    /// as long as somebody leaves a table browser open, and it would be inside
    /// whatever transaction the session has — which the trait says a cursor is
    /// not. So it takes its own connection, carries a canceller of its own, and
    /// is left out of the session's registry, because `MySqlSource::cancel`
    /// reaching it would mean a Cancel pressed over the query editor stopping
    /// that table browser as well.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, MySqlError> {
        let (conn, id) = open(&self.opts).await?;
        let reader = read(Held::Own(conn), sql, batch_rows).await?;
        Ok(Cursor {
            canceller: CursorCancel {
                opts: self.opts.clone(),
                id,
            },
            reader,
        })
    }

    /// Whether a transaction opened here survives to the next statement.
    ///
    /// Two things have to be true, and this driver is the only one where they
    /// come apart. Statements have to share a connection, which they do. And the
    /// server has to have the steps `TxStep` names, which is asked at connect
    /// rather than assumed — see `metadata::probe`. The same driver reaches
    /// MySQL, TiDB and StarRocks, and the third answers no.
    pub fn transactional(&self) -> bool {
        self.caps.transactions
    }

    /// Takes one step of transaction control on the session connection.
    ///
    /// On the session and not on a spare, which is the whole reason this driver
    /// holds one: a transaction belongs to a connection, so a `BEGIN` sent down
    /// a borrowed one opens a transaction the next statement will not be given
    /// and nobody can commit.
    ///
    /// Waits for the connection rather than reaching past whatever has it, so a
    /// step issued while a result is still streaming arrives after that result
    /// instead of in the middle of it. That is the only order that means
    /// anything: a `COMMIT` overtaking the statement it was meant to commit
    /// would commit less than the user watched happen.
    ///
    /// MySQL spells all six the standard way, which is not true of every
    /// database — the words live here rather than in the caller for that reason.
    /// None of them is checked against `transactional` first: where the server
    /// does not have a step it refuses the statement itself, in its own words,
    /// which say more than anything this side could write.
    pub async fn transaction(&self, step: &TxStep) -> Result<(), MySqlError> {
        let statement = match step {
            TxStep::Begin => "BEGIN".to_string(),
            TxStep::Commit => "COMMIT".to_string(),
            TxStep::Rollback => "ROLLBACK".to_string(),
            TxStep::Savepoint(name) => format!("SAVEPOINT {name}"),
            TxStep::RollbackTo(name) => format!("ROLLBACK TO SAVEPOINT {name}"),
            TxStep::Release(name) => format!("RELEASE SAVEPOINT {name}"),
        };
        let mut session = self.session.lock().await;
        // Listed for the length of the step and no longer, as a statement is. A
        // `COMMIT` can sit waiting on a lock another session holds, and a Cancel
        // that could not reach it would leave a frozen window with nothing to
        // press.
        let _live = register(&self.live, self.session_id);
        session.query_drop(statement).await?;
        Ok(())
    }

    /// Databases on this server, which is the level MySQL calls both a schema
    /// and a database and the level the navigator hangs relations off.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::schemas(&mut spare.conn).await;
        self.release(spare, &out);
        out
    }

    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::relations(&mut spare.conn, schema).await;
        self.release(spare, &out);
        out
    }

    pub async fn table_info(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<InfoField>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::table_info(&mut spare.conn, schema, relation).await;
        self.release(spare, &out);
        out
    }

    pub async fn routines(&self, schema: &str) -> Result<Vec<RoutineInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::routines(&mut spare.conn, schema).await;
        self.release(spare, &out);
        out
    }

    pub async fn routine_definition(
        &self,
        schema: &str,
        id: &str,
    ) -> Result<Option<String>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::routine_definition(&mut spare.conn, schema, id).await;
        self.release(spare, &out);
        out
    }

    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::columns(&mut spare.conn, schema, relation).await;
        self.release(spare, &out);
        out
    }

    /// The body of a view; `None` for anything that is not one.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::definition(&mut spare.conn, schema, relation).await;
        self.release(spare, &out);
        out
    }

    pub async fn indexes(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<IndexInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::indexes(&mut spare.conn, schema, relation, &self.caps).await;
        self.release(spare, &out);
        out
    }

    pub async fn unique_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::unique_keys(&mut spare.conn, schema, relation, &self.caps).await;
        self.release(spare, &out);
        out
    }

    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::foreign_keys(&mut spare.conn, schema, relation).await;
        self.release(spare, &out);
        out
    }

    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::referenced_by(&mut spare.conn, schema, relation).await;
        self.release(spare, &out);
        out
    }

    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::constraints(&mut spare.conn, schema, relation, &self.caps).await;
        self.release(spare, &out);
        out
    }

    pub async fn triggers(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<TriggerInfo>, MySqlError> {
        let mut spare = self.acquire().await?;
        let out = metadata::triggers(&mut spare.conn, schema, relation).await;
        self.release(spare, &out);
        out
    }
}

/// The connection one result is read on, held until the last page.
///
/// A query borrows the session's, so that a `BEGIN` from an earlier statement is
/// still in force. A cursor takes one of its own, for the reasons
/// `MySqlSource::cursor` gives. Either way the reading task holds it outright,
/// because `mysql_async`'s result borrows its connection for as long as it is
/// being read.
enum Held {
    Session {
        conn: OwnedMutexGuard<Conn>,
        /// Held rather than read, and kept in here rather than alongside so
        /// that the two cannot end separately: an id left in the registry after
        /// the statement finished names an idle connection, which on TiDB is
        /// not the harmless thing it is on MySQL.
        _live: Registration,
    },
    /// A cursor's own, named by nothing but the canceller it was handed out
    /// with, which is why there is no registration to hold here.
    Own(Conn),
}

impl Deref for Held {
    type Target = Conn;

    fn deref(&self) -> &Conn {
        match self {
            Held::Session { conn, .. } => conn,
            Held::Own(conn) => conn,
        }
    }
}

impl DerefMut for Held {
    fn deref_mut(&mut self) -> &mut Conn {
        match self {
            Held::Session { conn, .. } => conn,
            Held::Own(conn) => conn,
        }
    }
}

/// Hands a connection to a task and waits for the columns.
///
/// The connection arrives already chosen, because who it belongs to is the whole
/// difference between a query and a cursor and it is not a decision to make
/// twice.
async fn read(conn: Held, sql: &str, batch_rows: usize) -> Result<Reader, MySqlError> {
    let (schema_tx, schema_rx) = oneshot::channel();
    // Capacity one, so a reader that stops reading stops the producer rather
    // than letting it run ahead into memory. The bound on what a result costs is
    // this number times the batch size.
    let (pages_tx, pages_rx) = mpsc::channel(1);
    tokio::spawn(pump(conn, sql.to_string(), batch_rows, schema_tx, pages_tx));

    let schema = match schema_rx.await {
        Ok(result) => result?,
        // The task cannot end before answering unless it panicked, and a panic
        // there has already been reported by the runtime.
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
    })
}

/// Opens a connection and reads the number that stops it.
///
/// The id comes from `SELECT CONNECTION_ID()` and not from the handshake, which
/// `mysql_async` exposes for free as a `u32`. One round trip buys correctness on
/// a large TiDB cluster, where connection ids widen to 64 bits and a truncated
/// one names a different session — killing somebody else's statement, or
/// nothing.
async fn open(opts: &Opts) -> Result<(Conn, u64), MySqlError> {
    let mut conn = Conn::new(opts.clone()).await?;
    let id: u64 = conn
        .query_first("SELECT CONNECTION_ID()")
        .await?
        .unwrap_or_else(|| u64::from(conn.id()));
    Ok((conn, id))
}

fn register(live: &Arc<Mutex<Vec<u64>>>, id: u64) -> Registration {
    live.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    Registration {
        live: Arc::clone(live),
        id,
    }
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
///
/// The connection goes back whichever way this ends, including a failure and a
/// reader that walked away: `conn` is dropped with the task, and dropping a
/// `Held::Session` is what lets the next statement have the session.
async fn pump(
    mut conn: Held,
    sql: String,
    batch_rows: usize,
    schema_out: oneshot::Sender<Result<SchemaRef, MySqlError>>,
    pages: mpsc::Sender<Page>,
) {
    let prepared = match conn.prep(&*sql).await {
        Ok(prepared) => prepared,
        Err(e) => {
            let _ = schema_out.send(Err(e.into()));
            return;
        }
    };

    // Types are normally known here, before a row exists, because the server
    // answers a prepare with the column definitions, and a statement that
    // returns no result set — an UPDATE, a CREATE — describes zero columns,
    // which is the honest schema for it.
    //
    // `SHOW CREATE TABLE` is described the same way and is not that statement:
    // MySQL 8 answers its prepare with nothing and sends the two columns with
    // the execution. So a description of nothing is not believed until the
    // statement has run — which costs those statements the early types, and
    // costs every other statement nothing. Trusting the prepare instead would
    // report a `SHOW` as a write that changed no rows, and the DDL a table's
    // Structure tab is asking for would arrive as an empty result.
    let (schema, names, mut result) = if prepared.columns().is_empty() {
        let result = match conn.exec_iter(&prepared, ()).await {
            Ok(result) => result,
            // Nothing has been said about the columns yet, so this failure goes
            // to whoever is waiting for them rather than into a page stream
            // that nobody is reading, which is where a prepare failure goes.
            Err(e) => {
                let _ = schema_out.send(Err(e.into()));
                return;
            }
        };
        let columns = result.columns().unwrap_or_else(|| Arc::from([]));
        match columns_as_arrow(&columns) {
            Ok(described) => {
                if schema_out.send(Ok(Arc::clone(&described.0))).is_err() {
                    return;
                }
                (described.0, described.1, result)
            }
            Err(e) => {
                let _ = schema_out.send(Err(e));
                return;
            }
        }
    } else {
        let (schema, names) = match columns_as_arrow(&prepared.columns()) {
            Ok(described) => described,
            Err(e) => {
                let _ = schema_out.send(Err(e));
                return;
            }
        };
        if schema_out.send(Ok(Arc::clone(&schema))).is_err() {
            return;
        }
        let result = match conn.exec_iter(&prepared, ()).await {
            Ok(result) => result,
            Err(e) => {
                let _ = pages.send(Page::Failed(e.into())).await;
                return;
            }
        };
        (schema, names, result)
    };

    // Whether this statement reads or writes, which decides what its count
    // means. Taken from the column list rather than from whether a row stream
    // came back: `QueryResult::stream` answers `Some` for an `INSERT` too, with
    // a result set of no columns, so using it as the test would report every
    // write as having changed nothing.
    let reads = !schema.fields().is_empty();
    let mut produced = 0u64;

    {
        let rows = match result.stream::<Row>().await {
            Ok(rows) => rows,
            Err(e) => {
                let _ = pages.send(Page::Failed(e.into())).await;
                return;
            }
        };
        if let Some(mut rows) = rows {
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
    // What the number counts depends on the statement, which is what the trait
    // says: rows produced for one that reads, rows changed for one that writes.
    let affected = if reads {
        produced
    } else {
        result.affected_rows()
    };
    drop(result);

    let _ = pages.send(Page::Done(affected)).await;
}

/// The Arrow schema a column list describes, and the MySQL type names beside it.
///
/// The names come back with the schema because the row loop needs them for its
/// error messages and an Arrow field no longer carries them: reading them off
/// `schema.fields()` was the same list until a statement could be described
/// twice, and a helper that returns half of what it computed invites the caller
/// to recompute the other half from the wrong one.
fn columns_as_arrow(
    columns: &[mysql_async::Column],
) -> Result<(SchemaRef, Vec<String>), MySqlError> {
    let specs: Vec<ColumnType> = columns.iter().map(ColumnType::of).collect();
    let fields = columns
        .iter()
        .zip(&specs)
        .map(|(column, spec)| arrow_map::arrow_field(&column.name_str(), spec))
        .collect::<Result<Vec<_>, _>>()?;
    let schema = Arc::new(Schema::new(fields));
    let names = schema
        .fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    Ok((schema, names))
}

/// The reading half, shared by a result and a cursor because on this database
/// they are the same thing read at different speeds.
struct Reader {
    schema: SchemaRef,
    pages: mpsc::Receiver<Page>,
    rows_affected: Option<u64>,
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
