//! Phase 0 PostgreSQL read path: connect, execute, stream Arrow record batches.
//!
//! Deliberately narrow. There is no `Driver` trait here — with one driver, the
//! abstraction would be invented rather than derived. Phase 1 defines it once
//! there are two implementations to derive it from.

mod arrow_map;
mod metadata;

pub use metadata::{
    ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo,
};

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use arrow_map::{ColBuilder, ColumnType, arrow_field};
use futures_util::StreamExt;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::types::ToSql;
use tokio_postgres::{CancelToken, Client, NoTls, RowStream};

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
    /// One cancel token per connection this session has ever opened, kept
    /// because `cancel` has to name a backend and the connection running the
    /// statement may be checked out of the pool and unreachable through it.
    cancels: Arc<Mutex<Vec<CancelToken>>>,
}

/// A pooled connection, borrowed for one call and returned when it goes out of scope.
struct AcquiredConnection {
    client: Option<Client>,
    pool: Arc<Mutex<Vec<Client>>>,
    _permit: OwnedSemaphorePermit,
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
        let client = self.client.take().unwrap();
        tokio::spawn(async move {
            let mut pool_guard = pool.lock().await;
            pool_guard.push(client);
        });
    }
}

impl PgSource {
    pub async fn connect(conn_str: &str) -> Result<Self, PgError> {
        // Open one connection eagerly to ensure connection errors are caught early
        // This maintains the existing behavior where connection failures are reported
        // immediately rather than at first query time
        let (session, connection) = tokio_postgres::connect(conn_str, NoTls).await?;
        // The connection future drives the socket and must outlive us. Phase 0
        // has no reconnect story; a dropped connection surfaces as a query error.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres connection closed: {e}");
            }
        });

        // Start the pool empty. The session connection is already open, so a bad
        // password still fails at connect, which is the property that mattered.
        // The pool can open its first connection when a metadata call first needs one,
        // which acquire_connection already does.
        let pool = Arc::new(Mutex::new(Vec::new()));
        let semaphore = Arc::new(Semaphore::new(4));
        let cancels = Arc::new(Mutex::new(vec![session.cancel_token()]));

        Ok(Self {
            session,
            pool,
            semaphore,
            conn_str: conn_str.to_string(),
            cancels,
        })
    }

    /// Acquire a connection from the pool. This will block if all connections
    /// are busy until one becomes available.
    async fn acquire_connection(&self) -> Result<AcquiredConnection, PgError> {
        // Acquire a permit from the semaphore to limit concurrent access
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| PgError::PoolExhausted)?;

        // Try to get an existing connection from the pool
        {
            let mut pool = self.pool.lock().await;
            if let Some(client) = pool.pop() {
                return Ok(AcquiredConnection {
                    client: Some(client),
                    pool: Arc::clone(&self.pool),
                    _permit: permit,
                });
            }
        }

        // If no connection available, create a new one
        let (client, connection) = tokio_postgres::connect(&self.conn_str, NoTls).await?;
        // Registered before it is handed out, so a statement can never be running
        // on a connection `cancel` does not know about.
        self.cancels.lock().await.push(client.cancel_token());
        // The connection future drives the socket and must outlive us. Phase 0
        // has no reconnect story; a dropped connection surfaces as a query error.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres connection closed: {e}");
            }
        });

        Ok(AcquiredConnection {
            client: Some(client),
            pool: Arc::clone(&self.pool),
            _permit: permit,
        })
    }

    /// Asks the server to abandon whatever this session is currently running.
    ///
    /// The request travels on a connection of its own, which is why this can be
    /// called while a socket is busy streaming a result: the protocol has no way
    /// to interleave one, so a cancel sent in-band would sit in the queue behind
    /// the statement it is trying to stop.
    ///
    /// Every connection is named, not just the session, because a session owns
    /// several and the caller cannot see which one is busy: statements run on the
    /// session, metadata reads run on whichever connection the pool handed out,
    /// and a pooled connection in use is not in the pool to be found. Naming an
    /// idle backend costs a round trip and does nothing, which is the price of
    /// not having to know. A cursor is the exception — it carries its own
    /// canceller, because it is handed to the caller and outlives the call that
    /// made it.
    ///
    /// Best-effort by design. The server may finish before the request lands, or
    /// the statement may be between commands with nothing to cancel, and neither
    /// is an error — success here means the requests were delivered, not that
    /// anything was interrupted. What actually happened shows up as the running
    /// statement failing with `is_cancelled`, or not failing at all.
    pub async fn cancel(&self) -> Result<(), PgError> {
        // Cloned out from under the lock: a cancel is a round trip, and holding
        // the registry across all of them would block the connection being opened
        // by whatever we are trying to cancel.
        let tokens = self.cancels.lock().await.clone();
        // Every connection is asked before the first refusal is reported, so one
        // dropped connection cannot spare the rest.
        let results =
            futures_util::future::join_all(tokens.iter().map(|t| t.cancel_query(NoTls))).await;
        for result in results {
            result?;
        }
        Ok(())
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
        let (client, connection) = tokio_postgres::connect(&self.conn_str, NoTls).await?;
        // The connection future drives the socket and must outlive us.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres cursor connection closed: {e}");
            }
        });

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
        })
    }
}

pub struct ArrowStream {
    schema: SchemaRef,
    types: Vec<ColumnType>,
    rows: Pin<Box<RowStream>>,
    batch_rows: usize,
    exhausted: bool,
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
}

/// Asks the server to stop what a cursor is fetching.
///
/// Separate from the cursor so the two can be held at once: cancelling means
/// reaching the connection while a fetch has it, and the request goes out on a
/// connection of its own because the protocol cannot interleave one.
pub struct CursorCancel {
    token: CancelToken,
}

impl CursorCancel {
    /// Delivered is not interrupted. A fetch that had already finished leaves
    /// nothing to stop and this still succeeds; what actually happened shows up
    /// as the fetch failing with `is_cancelled`.
    pub async fn cancel(&self) -> Result<(), PgError> {
        self.token.cancel_query(NoTls).await?;
        Ok(())
    }
}

impl Cursor {
    /// Fetch the next batch of rows from the cursor.
    ///
    /// Returns `Ok(None)` when the cursor has reached the end of the result set.
    /// Returns an error if the fetch fails.
    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, PgError> {
        // Use FETCH FORWARD to get the next batch of rows
        let sql = format!(
            "FETCH FORWARD {} FROM {}",
            self.batch_rows, self.cursor_name
        );
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

impl Drop for ArrowStream {
    fn drop(&mut self) {
        // When the stream is dropped, we don't need to return anything to the pool
        // because we used the session connection directly, not a pooled connection
        // The session connection is kept alive for the lifetime of the PgSource
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
