//! Phase 0 PostgreSQL read path: connect, execute, stream Arrow record batches.
//!
//! Deliberately narrow. There is no `Driver` trait here — with one driver, the
//! abstraction would be invented rather than derived. Phase 1 defines it once
//! there are two implementations to derive it from.

mod arrow_map;
mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use arrow_map::{ColBuilder, ColumnType, arrow_field};
use dbconn::{
    ColumnInfo, ConstraintInfo, DatabaseInfo, IndexInfo, RelationInfo, RelationshipInfo,
    RoutineInfo, SchemaInfo, SequenceInfo, TriggerInfo, TxStep, UniqueKeyInfo,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::types::ToSql;
use tokio_postgres::{CancelToken, Client, RowStream};

mod tls;
pub use tls::SslMode;
use tls::Tls;

#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("{}", describe(.0))]
    Postgres(#[from] tokio_postgres::Error),
    #[error("column {column:?} has unsupported type {pg_type}")]
    UnsupportedType { column: String, pg_type: String },
    #[error("numeric value {0} does not fit the column's fixed scale")]
    NumericOverflow(String),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("connection pool exhausted")]
    PoolExhausted,
    #[error("sslmode={0} is not one of disable, allow, prefer, require, verify-ca or verify-full")]
    UnknownSslMode(String),
    #[error("the CA certificate at {path} could not be read: {reason}")]
    RootCertificate { path: String, reason: String },
    #[error("TLS could not be set up: {0}")]
    Tls(String),
}

impl PgError {
    /// Where in the statement the server says the trouble is: a 1-based index,
    /// counted in characters, into the SQL that was sent.
    ///
    /// The message alone says what is wrong and never where, which for a syntax
    /// error is most of the answer missing. A front end that has the number can
    /// put the caret on the character.
    ///
    /// `Internal` positions are dropped rather than passed on. They index a
    /// query the server generated on our behalf — a PL/pgSQL body, say — not the
    /// text we handed it, so applying one to an editor points confidently at the
    /// wrong character. No position is better than a wrong one.
    pub fn statement_position(&self) -> Option<u32> {
        let PgError::Postgres(e) = self else {
            return None;
        };
        match e.as_db_error()?.position()? {
            ErrorPosition::Original(p) => Some(*p),
            ErrorPosition::Internal { .. } => None,
        }
    }

    /// Whether the server stopped this statement because somebody asked it to.
    ///
    /// A cancelled statement fails like any other, and the difference matters to
    /// whoever is looking at the screen: "canceling statement due to user
    /// request" in an error banner reads as a fault, when it is the button they
    /// just pressed working. The caller having issued the cancel is not enough
    /// to tell them apart — a statement can fail on its own merits in the same
    /// moment — so the answer comes from the SQLSTATE the server sent rather
    /// than from what this side happens to remember doing.
    pub fn is_cancelled(&self) -> bool {
        let PgError::Postgres(e) = self else {
            return false;
        };
        e.as_db_error()
            .is_some_and(|db| *db.code() == SqlState::QUERY_CANCELED)
    }
}

/// An error that never reached the server, with the reason it did not.
///
/// A failure before the connection exists carries no `DbError`, and what
/// tokio-postgres displays for one names the stage rather than the cause:
/// "error connecting to server" is every possible connection failure at once —
/// wrong port, no route, no server, TLS refused — and a connection dialog
/// showing it leaves the user to guess which. The reason is in the source
/// chain, so the chain is what gets rendered.
fn with_causes(e: &tokio_postgres::Error) -> String {
    use std::error::Error;
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(next) = cause {
        out.push_str(": ");
        out.push_str(&next.to_string());
        cause = next.source();
    }
    out
}

/// Renders a driver error the way the server stated it.
///
/// `tokio_postgres::Error` displays as the bare string "db error"; everything a
/// user needs is in the attached `DbError`. Without this the UI surfaces an
/// error banner that says nothing, which is worse than no banner.
fn describe(e: &tokio_postgres::Error) -> String {
    let Some(db) = e.as_db_error() else {
        return with_causes(e);
    };
    let mut out = db.message().to_string();
    if let Some(detail) = db.detail() {
        out.push_str(" — ");
        out.push_str(detail);
    }
    if let Some(hint) = db.hint() {
        out.push_str(" (");
        out.push_str(hint);
        out.push(')');
    }
    out
}

/// The cancel tokens of every connection currently running a statement, keyed
/// by checkout. See `PgSource::busy`.
type Registry = Arc<Mutex<HashMap<u64, CancelToken>>>;

/// A connection to one PostgreSQL database, plus spare connections for looking things up.
///
/// Statements run on the session connection and nothing else does, because a transaction
/// spans statements and a cancellation has to name a backend — both are properties of one
/// connection, and neither survives being handed a different one. Metadata reads belong to
/// no transaction and answer quickly, so they take a connection from the pool instead, and
/// expanding a schema stops queueing behind a result that is still streaming.
pub struct PgSource {
    session: Client,
    pool: Arc<Mutex<Vec<Client>>>,
    semaphore: Arc<Semaphore>,
    conn_str: String,
    /// What was decided about the wire, kept because every later connection has
    /// to make the same decision — the pool's, the cursor's, and the one a
    /// cancel opens for itself.
    tls: Tls,
    /// The connections a statement is on right now, by checkout.
    ///
    /// `cancel` has to name a backend and the one running the statement may be
    /// checked out of the pool and unreachable through it, so the registry is
    /// kept here rather than derived. Only the busy ones, because the request
    /// is not free: sent to an idle backend it arrives after that connection
    /// has moved on and stops whatever it moved on to.
    busy: Registry,
    /// Keys are handed out rather than taken from the connection, because the
    /// same connection can be checked out again before the previous entry's
    /// removal — which drop can only spawn — has run.
    next_checkout: AtomicU64,
    /// Number of times `cancel` has been called since this `PgSource` was
    /// created.
    ///
    /// A counter rather than a flag: a flag set by a cancel that landed long
    /// ago cannot be distinguished from one that happened during the current
    /// borrow, so a reader could never tell whether the connection it was
    /// handed was still possibly-cancelled. The counter lets a reader compare
    /// the value at checkout with the value at drop — if they differ, a cancel
    /// happened somewhere in between, and the connection must be retired.
    cancellations: Arc<AtomicU64>,
}

/// A pooled connection, borrowed for one call and returned when it goes out of scope.
struct AcquiredConnection {
    client: Option<Client>,
    pool: Arc<Mutex<Vec<Client>>>,
    _permit: OwnedSemaphorePermit,
    busy: Busy,
    /// Shared counter that `cancel` bumps.
    cancellations: Arc<AtomicU64>,
    /// Value of `cancellations` at the moment this connection was checked out.
    /// If the current value differs at drop, a cancel happened during the
    /// borrow and the connection must be retired.
    cancellations_at_checkout: u64,
}

impl Deref for AcquiredConnection {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        // client is only ever taken by drop, so it is Some for the whole life
        // of any reference a caller can hold
        self.client.as_ref().unwrap()
    }
}

impl DerefMut for AcquiredConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // client is only ever taken by drop, so it is Some for the whole life
        // of any reference a caller can hold
        self.client.as_mut().unwrap()
    }
}

impl Drop for AcquiredConnection {
    fn drop(&mut self) {
        // This must be in a spawned task because drop cannot await
        let pool = Arc::clone(&self.pool);
        let cancellations = Arc::clone(&self.cancellations);
        let cancellations_at_checkout = self.cancellations_at_checkout;
        let client = self.client.take().unwrap();
        // Taken over from the guard so both happen in one task, in this order.
        // Left to run itself the removal is a second, unordered task, and a
        // token still in the registry after the connection is back in the pool
        // is one a cancel can aim at whoever borrows it next.
        let entry = self.busy.take_entry();
        tokio::spawn(async move {
            if let Some((id, busy)) = entry {
                busy.lock().await.remove(&id);
            }
            // If a cancel happened between checkout and now, the connection
            // may still carry an unlanded signal. Pushing it back into the pool
            // risks killing the next borrower's statement, so close it instead.
            // The pool refills itself on demand in `acquire_connection`, so
            // losing a connection costs one reconnect and nothing else.
            //
            // Read after the removal above, so a cancel that could still find
            // this connection is one whose count this load already includes.
            if cancellations.load(Ordering::SeqCst) != cancellations_at_checkout {
                drop(client);
                return;
            }
            let mut pool_guard = pool.lock().await;
            pool_guard.push(client);
        });
    }
}

/// Holds a connection in `PgSource::busy` for as long as it lives.
///
/// A guard rather than a pair of calls because what has to be true is a span,
/// not two events: the token must be findable for exactly as long as the
/// statement can still be stopped. Every way out of that span — the borrow
/// ending, the stream being dropped, an error unwinding past it — is a drop.
struct Busy {
    id: u64,
    /// `None` once a holder has taken the removal over, which is what stops
    /// this guard from spawning a second one behind their back.
    busy: Option<Registry>,
    /// Taken at construction, because `tokio::spawn` panics off the runtime and
    /// this guard is dropped wherever its holder is. An `ArrowStream` is handed
    /// across the FFI and freed by the front end on a thread that has never
    /// been inside one — a bare `spawn` in `Drop` kills the process there.
    runtime: tokio::runtime::Handle,
}

impl Busy {
    /// Hands the removal to a caller that will perform it themselves.
    ///
    /// For a caller that has something else to order against it. `Drop` can
    /// only spawn, and a spawned removal is ordered against nothing.
    fn take_entry(&mut self) -> Option<(u64, Registry)> {
        self.busy.take().map(|busy| (self.id, busy))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let Some(busy) = self.busy.take() else { return };
        // This must be in a spawned task because drop cannot await
        let id = self.id;
        self.runtime.spawn(async move {
            let mut busy_guard = busy.lock().await;
            busy_guard.remove(&id);
        });
    }
}

impl PgSource {
    pub async fn connect(conn_str: &str) -> Result<Self, PgError> {
        // Taken out of the string before tokio-postgres is handed it: two of
        // libpq's five spellings are ones it refuses to parse, and `sslrootcert`
        // is an option it has never heard of. `tls::split_ssl` says more.
        let (conn_str, mode, root_cert) = tls::split_ssl(conn_str)?;
        let tls = Tls::new(mode, root_cert.as_deref())?;
        // Open one connection eagerly to ensure connection errors are caught early
        // This maintains the existing behavior where connection failures are reported
        // immediately rather than at first query time
        let session = tls.connect(&conn_str, "connection").await?;

        // Start the pool empty. The session connection is already open, so a bad
        // password still fails at connect, which is the property that mattered.
        // The pool can open its first connection when a metadata call first needs one,
        // which acquire_connection already does.
        let pool = Arc::new(Mutex::new(Vec::new()));
        let semaphore = Arc::new(Semaphore::new(4));
        // The session's token is not registered here. It belongs in the registry
        // only while a statement is on it, and connecting is not that.
        let busy = Arc::new(Mutex::new(HashMap::new()));
        let cancellations = Arc::new(AtomicU64::new(0));

        Ok(Self {
            session,
            pool,
            semaphore,
            conn_str,
            tls,
            busy,
            next_checkout: AtomicU64::new(0),
            cancellations,
        })
    }

    /// Registers `token` as busy for as long as the returned guard is held, and
    /// reports the cancellation count as of that moment.
    ///
    /// Both under one lock, which is the whole reason they are one call. Split
    /// apart, a cancel can land between them: it finds the token and signals
    /// this connection, and the count read afterwards already includes it, so
    /// at drop the two agree and a signalled connection goes back into the pool
    /// looking untouched — the one outcome this registry exists to prevent.
    async fn mark_busy(&self, token: CancelToken) -> (Busy, u64) {
        let id = self.next_checkout.fetch_add(1, Ordering::SeqCst);
        let mut busy = self.busy.lock().await;
        busy.insert(id, token);
        let cancellations = self.cancellations.load(Ordering::SeqCst);
        drop(busy);
        (
            Busy {
                id,
                busy: Some(Arc::clone(&self.busy)),
                runtime: tokio::runtime::Handle::current(),
            },
            cancellations,
        )
    }

    /// Acquire a connection from the pool. This will block if all connections
    /// are busy until one becomes available.
    async fn acquire_connection(&self) -> Result<AcquiredConnection, PgError> {
        // Acquire a permit from the semaphore to limit concurrent access
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| PgError::PoolExhausted)?;

        // Try to get an existing connection from the pool. The guard is released
        // before registering, so the two locks are never held at once.
        let pooled = self.pool.lock().await.pop();
        if let Some(client) = pooled {
            let (busy, cancellations_at_checkout) = self.mark_busy(client.cancel_token()).await;
            return Ok(AcquiredConnection {
                client: Some(client),
                pool: Arc::clone(&self.pool),
                _permit: permit,
                busy,
                cancellations: Arc::clone(&self.cancellations),
                cancellations_at_checkout,
            });
        }

        // If no connection available, create a new one
        let client = self.tls.connect(&self.conn_str, "connection").await?;

        // Registered before it is handed out, so a statement can never be running
        // on a connection `cancel` does not know about.
        let (busy, cancellations_at_checkout) = self.mark_busy(client.cancel_token()).await;

        Ok(AcquiredConnection {
            client: Some(client),
            pool: Arc::clone(&self.pool),
            _permit: permit,
            busy,
            cancellations: Arc::clone(&self.cancellations),
            cancellations_at_checkout,
        })
    }

    /// Asks the server to abandon whatever this session is currently running,
    /// and answers how many backends were named.
    ///
    /// The request travels on a connection of its own, which is why this can be
    /// called while a socket is busy streaming a result: the protocol has no way
    /// to interleave one, so a cancel sent in-band would sit in the queue behind
    /// the statement it is trying to stop.
    ///
    /// Every connection with a statement on it is named, not just the session,
    /// because a session runs on several and the caller cannot see which one is
    /// busy: statements run on the session, metadata reads run on whichever
    /// connection the pool handed out, and a pooled connection in use is not in
    /// the pool to be found. Idle ones are left out, because naming one is not
    /// free: this call returns when the postmaster accepts the request, and the
    /// signal arrives at the backend afterwards — by which time an idle
    /// connection has been handed to somebody else, and their statement is what
    /// stops. A cursor is the exception — it carries its own canceller, because
    /// it is handed to the caller and outlives the call that made it.
    ///
    /// Best-effort by design. The count is how many were asked, not how many
    /// stopped: the server may finish before the request lands, or the statement
    /// may be between commands with nothing to cancel, and neither is an error.
    /// What actually happened shows up as the running statement failing with
    /// `is_cancelled`, or not failing at all. Zero is the one that carries
    /// information — there was nothing running to stop.
    pub async fn cancel(&self) -> Result<usize, PgError> {
        // Cloned out from under the lock: a cancel is a round trip, and holding
        // the registry across all of them would block the connection being opened
        // by whatever we are trying to cancel.
        let tokens = {
            let busy = self.busy.lock().await;
            // Counted under the same lock the registry is read under, so a
            // checkout either registers early enough to be signalled here and
            // reads the lower count, or registers too late for both. See
            // `mark_busy` for what splitting the two costs.
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            busy.values().cloned().collect::<Vec<_>>()
        };
        // Every connection is asked before the first refusal is reported, so one
        // dropped connection cannot spare the rest.
        let results =
            futures_util::future::join_all(tokens.iter().map(|t| self.tls.cancel(t))).await;
        for result in results {
            result?;
        }
        Ok(tokens.len())
    }

    /// Takes one step of transaction control on the session connection.
    ///
    /// On the session and not on a pooled connection, which is the whole reason
    /// this driver keeps one: a transaction belongs to a connection, so a
    /// `BEGIN` sent down a borrowed one opens a transaction the next statement
    /// will not be given and nobody can commit.
    ///
    /// PostgreSQL spells all six of these the standard way, which is not true of
    /// every database — the words live here rather than in the caller for that
    /// reason.
    pub async fn transaction(&self, step: &TxStep) -> Result<(), PgError> {
        // The count is for connections that can be retired, and the session is
        // not one of them: it carries the open transaction, so closing it would
        // roll back the work the user is in the middle of.
        let (_busy, _) = self.mark_busy(self.session.cancel_token()).await;
        let statement = match step {
            TxStep::Begin => "BEGIN".to_string(),
            TxStep::Commit => "COMMIT".to_string(),
            TxStep::Rollback => "ROLLBACK".to_string(),
            TxStep::Savepoint(name) => format!("SAVEPOINT {name}"),
            TxStep::RollbackTo(name) => format!("ROLLBACK TO SAVEPOINT {name}"),
            TxStep::Release(name) => format!("RELEASE SAVEPOINT {name}"),
        };
        self.session.batch_execute(&statement).await?;
        Ok(())
    }

    /// The databases on this server, for the level above the navigator root.
    ///
    /// List-only. PostgreSQL cannot change database within a session, so opening
    /// one of these means opening another connection — which is what this
    /// method's caller does with the answer.
    pub async fn databases(&self) -> Result<Vec<DatabaseInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        metadata::databases(&conn).await
    }

    /// Non-system schemas, for the navigator root.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::schemas(&conn).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// Tables, views, and other relations within a schema.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::relations(&conn, schema).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// Functions and procedures within a schema, without their bodies.
    pub async fn routines(&self, schema: &str) -> Result<Vec<RoutineInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::routines(&conn, schema).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// Sequences within a schema, whole: there is no second call.
    pub async fn sequences(&self, schema: &str) -> Result<Vec<SequenceInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::sequences(&conn, schema).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// One routine's source, by the id `routines` reported.
    pub async fn routine_definition(
        &self,
        schema: &str,
        id: &str,
    ) -> Result<Option<String>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::routine_definition(&conn, schema, id).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// Column definitions for one relation.
    pub async fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::columns(&conn, schema, relation).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// The statement a view is defined by; `None` for a relation that has none.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::definition(&conn, schema, relation).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// Indexes on one relation, primary key first.
    pub async fn indexes(&self, schema: &str, relation: &str) -> Result<Vec<IndexInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::indexes(&conn, schema, relation).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// UNIQUE constraints on one relation, primary key excluded.
    pub async fn unique_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        metadata::unique_keys(&conn, schema, relation).await
    }

    /// Foreign keys declared by one relation.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::foreign_keys(&conn, schema, relation).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// Foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::referenced_by(&conn, schema, relation).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// CHECK, UNIQUE, and EXCLUDE constraints.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::constraints(&conn, schema, relation).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// User-defined triggers, excluding constraint enforcement machinery.
    pub async fn triggers(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<TriggerInfo>, PgError> {
        let conn = self.acquire_connection().await?;
        let result = metadata::triggers(&conn, schema, relation).await;
        // Connection is automatically returned to pool when conn goes out of scope
        result
    }

    /// Prepare `sql` and begin streaming results as Arrow batches of
    /// `batch_rows` rows.
    ///
    /// Resolves once the server acknowledges the bind, which is later than it
    /// reads: the server buffers its output and flushes at the end of the
    /// command, so on a slow statement this waits out the whole execution and
    /// then returns a stream whose first batch has already arrived. Execution
    /// failures — and a `cancel` that lands mid-statement — therefore still
    /// surface from `next_batch`, not from here.
    ///
    /// This runs on the session connection to ensure that statements share the
    /// same connection and transaction context, and that cancellation targets
    /// a specific backend.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, PgError> {
        // Registered here and not alongside the stream below: `query_raw` does
        // not resolve until the server has run the whole statement, so the
        // cancel most worth catching arrives while this call is still awaiting,
        // and would find nothing if the token went in afterwards.
        let (busy, _) = self.mark_busy(self.session.cancel_token()).await;
        let stmt = self.session.prepare(sql).await?;

        let types: Vec<ColumnType> = stmt
            .columns()
            .iter()
            .map(|c| ColumnType {
                pg_type: c.type_().clone(),
                modifier: c.type_modifier(),
            })
            .collect();
        let fields = stmt
            .columns()
            .iter()
            .zip(&types)
            .map(|(c, t)| arrow_field(c.name(), t))
            .collect::<Result<Vec<_>, _>>()?;
        let schema = Arc::new(Schema::new(fields));

        let no_params: [&(dyn ToSql + Sync); 0] = [];
        let rows = self
            .session
            .query_raw(&stmt, no_params.iter().copied())
            .await?;

        // Create ArrowStream with the session connection
        Ok(ArrowStream {
            schema,
            types,
            rows: Box::pin(rows),
            batch_rows,
            exhausted: false,
            _busy: busy,
        })
    }

    /// Open a cursor over `sql` and return a handle to fetch pages.
    ///
    /// A cursor occupies its connection while open, so the handle owns a
    /// connection of its own for the lifetime of the cursor. The connection
    /// is closed when the cursor is dropped, ensuring that no changes are
    /// committed.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, PgError> {
        // Create a fresh connection for the cursor
        let client = self
            .tls
            .connect(&self.conn_str, "cursor connection")
            .await?;

        // Create a unique cursor name using atomic counter
        static CURSOR_COUNTER: AtomicU64 = AtomicU64::new(0);
        let cursor_id = CURSOR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let cursor_name = format!("cursor_{}", cursor_id);

        // Begin transaction and declare the cursor
        client.batch_execute("BEGIN").await?;

        let declare_sql = format!("DECLARE {} CURSOR FOR {}", cursor_name, sql);
        client.batch_execute(&declare_sql).await?;

        // Prepare the schema information by fetching column info from the statement
        let stmt = client.prepare(sql).await?;

        let types: Vec<ColumnType> = stmt
            .columns()
            .iter()
            .map(|c| ColumnType {
                pg_type: c.type_().clone(),
                modifier: c.type_modifier(),
            })
            .collect();
        let fields = stmt
            .columns()
            .iter()
            .zip(&types)
            .map(|(c, t)| arrow_field(c.name(), t))
            .collect::<Result<Vec<_>, _>>()?;
        let schema = Arc::new(Schema::new(fields));

        Ok(Cursor {
            client,
            schema,
            types,
            batch_rows,
            cursor_name,
            tls: self.tls.clone(),
        })
    }
}

pub struct ArrowStream {
    schema: SchemaRef,
    types: Vec<ColumnType>,
    rows: Pin<Box<RowStream>>,
    batch_rows: usize,
    exhausted: bool,
    _busy: Busy,
}

/// A cursor over a PostgreSQL query result.
///
/// A cursor occupies its connection while open, so the handle owns a
/// connection of its own for the lifetime of the cursor. The connection
/// is closed when the cursor is dropped, ensuring that no changes are
/// committed.
pub struct Cursor {
    client: Client,
    schema: SchemaRef,
    types: Vec<ColumnType>,
    batch_rows: usize,
    cursor_name: String,
    /// Carried so that `canceller` can hand it on. The cursor itself never
    /// opens another socket.
    tls: Tls,
}

/// Asks the server to stop what a cursor is fetching.
///
/// Separate from the cursor so the two can be held at once: cancelling means
/// reaching the connection while a fetch has it, and the request goes out on a
/// connection of its own because the protocol cannot interleave one.
pub struct CursorCancel {
    token: CancelToken,
    /// The cancel opens a connection of its own, so it has to know what the
    /// cursor's connection negotiated. Sent in the clear to a server that
    /// requires TLS it is refused, and the fetch it was meant to stop runs on
    /// while the window reports it cancelled.
    tls: Tls,
}

impl CursorCancel {
    /// Delivered is not interrupted. A fetch that had already finished leaves
    /// nothing to stop and this still succeeds; what actually happened shows up
    /// as the fetch failing with `is_cancelled`.
    pub async fn cancel(&self) -> Result<(), PgError> {
        self.tls.cancel(&self.token).await
    }
}

impl Cursor {
    /// Fetch the next batch of rows from the cursor.
    ///
    /// Returns `Ok(None)` when the cursor has reached the end of the result set.
    /// Returns an error if the fetch fails.
    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, PgError> {
        // `FETCH n`, not `FETCH FORWARD n`. PostgreSQL treats the two as the
        // same statement — a bare count is forward by definition — but the word
        // is not free: GreptimeDB serves the PostgreSQL wire protocol and its
        // parser wants a literal count after FETCH, so `FORWARD` fails there and
        // paging a table is the whole of what a cursor is for. Dropping a word
        // that says what the default already says costs this driver nothing.
        let sql = format!("FETCH {} FROM {}", self.batch_rows, self.cursor_name);
        let rows = self.client.query(&sql, &[]).await?;

        if rows.is_empty() {
            return Ok(None);
        }

        let mut builders: Vec<ColBuilder> = self
            .types
            .iter()
            .map(|t| ColBuilder::new(t, self.batch_rows))
            .collect();

        let mut n = 0usize;
        for row in rows {
            for (idx, b) in builders.iter_mut().enumerate() {
                b.append(&row, idx)?;
            }
            n += 1;
        }

        if n == 0 {
            return Ok(None);
        }

        let arrays = builders.iter_mut().map(|b| b.finish()).collect();
        Ok(Some(RecordBatch::try_new(
            Arc::clone(&self.schema),
            arrays,
        )?))
    }

    /// The columns this cursor's rows arrive in.
    ///
    /// Known at declare time rather than at first fetch: the statement was
    /// prepared to build it, so a caller can lay out a grid before a single
    /// row has come back.
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// A handle for stopping this cursor's fetch from another thread.
    ///
    /// Taken out here rather than reached for at cancel time, because by then
    /// the cursor itself is borrowed by the `fetch` that is to be stopped —
    /// which is the whole situation. `PgSource::cancel` cannot do this job: it
    /// cancels the session connection, and a cursor runs on one of its own.
    pub fn canceller(&self) -> CursorCancel {
        CursorCancel {
            token: self.client.cancel_token(),
            tls: self.tls.clone(),
        }
    }

    /// Close the cursor explicitly.
    ///
    /// This is optional as the cursor will be closed automatically when dropped.
    pub async fn close(&mut self) -> Result<(), PgError> {
        let sql = format!("CLOSE {}", self.cursor_name);
        self.client.batch_execute(&sql).await?;
        // Rollback the transaction to close it properly
        self.client.batch_execute("ROLLBACK").await?;
        Ok(())
    }
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows the server said this statement affected, or `None` until the result
    /// has been read to the end.
    ///
    /// A statement that returns no result set — an `UPDATE`, a `CREATE` — still
    /// did something, and this count is the only thing it says about itself. It
    /// rides on the `CommandComplete` that terminates the result, so there is
    /// nothing to read until `next_batch` has answered `None`; a number reported
    /// before then would be a guess dressed as an answer.
    ///
    /// The verb does not come with it. tokio-postgres parses the trailing count
    /// out of the command tag and drops the rest, so `UPDATE 3` reaches us as 3
    /// and `CREATE TABLE` as 0. Recovering the verb by re-reading the SQL we
    /// sent would be this side inventing a fact the server did not state, which
    /// is how a `CREATE` ends up labelled by somebody's regex for `INSERT`.
    pub fn rows_affected(&self) -> Option<u64> {
        self.rows.rows_affected()
    }

    /// Next batch, or `None` once the result is fully consumed.
    ///
    /// Builders are allocated per batch. Reusing them across batches would save
    /// allocations but force a copy out of the shared buffer on `finish`, which
    /// is the opposite of what this path exists to demonstrate.
    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, PgError> {
        if self.exhausted {
            return Ok(None);
        }

        let mut builders: Vec<ColBuilder> = self
            .types
            .iter()
            .map(|t| ColBuilder::new(t, self.batch_rows))
            .collect();

        let mut n = 0usize;
        while n < self.batch_rows {
            match self.rows.next().await {
                Some(row) => {
                    let row = row?;
                    for (idx, b) in builders.iter_mut().enumerate() {
                        b.append(&row, idx)?;
                    }
                    n += 1;
                }
                None => {
                    self.exhausted = true;
                    break;
                }
            }
        }

        if n == 0 {
            return Ok(None);
        }

        let arrays = builders.iter_mut().map(|b| b.finish()).collect();
        Ok(Some(RecordBatch::try_new(
            Arc::clone(&self.schema),
            arrays,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONN: &str = "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench";

    async fn connect() -> PgSource {
        PgSource::connect(CONN)
            .await
            .expect("benchmark database unreachable; run `make db-seed`")
    }

    /// Needs no database — it needs the absence of one, which is why it can run
    /// in the unit suite. Port 1 is reserved and nothing on a developer machine
    /// or a CI runner listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let err = PgSource::connect("host=127.0.0.1 port=1 user=nobody dbname=nothing")
            .await
            .err()
            .expect("nothing is listening on port 1");
        let message = err.to_string();
        // The stage on its own — which is all tokio-postgres displays — fits
        // every connection failure there is, so a dialog showing it tells the
        // user nothing they did not already know from the dialog being up.
        assert!(
            message.len() > "error connecting to server".len(),
            "the message stops at the stage and never says the cause: {message}"
        );
        assert!(
            message.to_lowercase().contains("refused"),
            "expected the refusal to survive into the message, got: {message}"
        );
    }

    /// A cancelled pooled connection is discarded, not returned.
    ///
    /// `PgSource::cancel` sends a cancel request to every busy connection, and
    /// a connection that was cancelled while checked out must be dropped rather
    /// than handed to the next caller — otherwise a late-arriving cancel signal
    /// lands on somebody else's statement. The code does this by comparing a
    /// cancellation counter taken at checkout with the counter at drop (see
    /// `cancellations_at_checkout` and the `Drop` impl). Nothing proves it.
    #[tokio::test]
    #[ignore = "requires the benchmark database"]
    async fn a_cancelled_pooled_connection_is_discarded_not_returned() {
        let src = connect().await;

        // Control: acquire a connection, drop it without cancelling. The pool
        // must gain it back.
        let guard_control = src.acquire_connection().await.expect("acquire");
        let pool_before = src.pool.lock().await.len();
        drop(guard_control);
        // Drop returns the connection on a spawned task, so we need to wait
        // for it to land. Bounded retry rather than a blind sleep.
        let mut found = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let pool_after = src.pool.lock().await.len();
            if pool_after > pool_before {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "pool should have gained the connection back after normal drop"
        );

        // Now the real test: acquire, cancel, drop. The pool must NOT gain it
        // back because the Drop impl sees the counter changed and drops the
        // connection instead.
        let guard_cancel = src.acquire_connection().await.expect("acquire");
        let pool_before_cancel = src.pool.lock().await.len();
        src.cancel().await.expect("cancel");
        drop(guard_cancel);
        let mut not_found = true;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let pool_after_cancel = src.pool.lock().await.len();
            if pool_after_cancel > pool_before_cancel {
                not_found = false;
                break;
            }
        }
        assert!(
            not_found,
            "pool must not have gained the cancelled connection back"
        );
    }
}
