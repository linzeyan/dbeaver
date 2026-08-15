//! Apache Cassandra, behind the same `Driver` trait as the SQL databases.
//!
//! CQL looks enough like SQL that the interesting question is where it stops
//! being SQL, and this driver exists to find out. Most of the trait fits with
//! nothing bent. Four places did not, and they are the finding:
//!
//! **There is no cancellation, and this is the first driver that has to say so.**
//! PostgreSQL sends a cancel request on a second connection, MongoDB looks the
//! operation up in `$currentOp` and calls `killOp`, ClickHouse sends `KILL
//! QUERY`. Cassandra's native protocol has no message for it at all: once a
//! `QUERY` frame is on the wire the coordinator will produce the page whether
//! anybody is still reading or not. So `cancel` stops the read *on this side* —
//! the in-flight fetch resolves as a failure whose `is_cancelled` is true, and
//! nothing further is asked for. The server goes on gathering rows nobody will
//! look at, and it finishes when it finishes. That is the honest shape, and the
//! alternative was worse: a driver that dropped the connection to stop a
//! statement would trade one wasted page for a reconnect and a hole in whatever
//! else the session was doing.
//!
//! **The cursor came free, and it is the only property that did.** Cassandra's
//! paging state is exactly what `Driver::cursor` asks for — page *n* costs what
//! page one costs, because the state names where the coordinator stopped rather
//! than how far in it should skip. So `LIMIT`/`OFFSET`, which the trait exists
//! instead of, is not even reached for. One caveat with teeth: a paging state is
//! a *position*, not a snapshot. Nothing already read comes back and nothing
//! before the position is skipped, which is what the trait requires; a row
//! inserted ahead of the position after paging began will be read, because by
//! then it is simply a row that is there. PostgreSQL's cursor would not have
//! shown it. Neither is wrong and only one of them is Cassandra.
//!
//! **A browse cannot be given a total order, and the trait's usual answer makes
//! it fail outright.** `ORDER BY` in CQL is legal only on clustering columns
//! within a single partition. Appending the primary key — which is what makes
//! every other database's browse look the same twice — turns a working statement
//! into `ORDER BY is only supported when the partition key is restricted by an
//! EQ or an IN`. So `browse` ignores `what.keys`, writes its own statement
//! rather than calling `Browse::sql`, and the rows arrive in token order: stable
//! within one read, arbitrary between two. See `driver.rs`.
//!
//! **Four metadata calls are structurally empty and one class of object is
//! missing entirely.** Cassandra declares no foreign keys and has no
//! constraints, so `foreign_keys`, `referenced_by` and `constraints` answer with
//! nothing and issue no query to find out — see `metadata.rs`, where each says
//! so on its own. `transactional` is false: a lightweight transaction is one
//! statement's compare-and-set and a `BATCH` is atomic, but neither is something
//! a session opens now and commits later, which is the only thing the trait's
//! `TxStep` describes.
//!
//! One thing came out better than expected. Cassandra reports a syntax error as
//! `line 1:34 no viable alternative at input 'ORDER'`, and the column is counted
//! in **characters** rather than bytes — the opposite of ClickHouse, and the
//! reason `position` below has no byte arithmetic in it.

mod arrow_map;
mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow_map::Plan;
use async_trait::async_trait;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::errors::{
    ExecutionError, IntoRowsResultError, NewSessionError, RequestAttemptError, TranslationError,
};
use scylla::policies::address_translator::{AddressTranslator, UntranslatedPeer};
use scylla::response::{PagingState, PagingStateResponse};
use scylla::statement::unprepared::Statement;
use scylla::value::Row;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Notify;

/// Cassandra's default port, used when a connection string names none.
const DEFAULT_PORT: u16 = 9042;

/// What a stopped read says, in one place because it is said in two.
///
/// The second half of the sentence is the part that matters and the part no
/// other driver has to write: this is a client-side stop, so the work does not
/// end when the button is pressed.
const STOPPED: &str = "the read was stopped here; Cassandra has no server-side cancel, \
                       so the coordinator finishes the page nobody will read";

#[derive(Debug, thiserror::Error)]
pub enum CassandraError {
    /// The session could never be opened: a host that does not resolve, a
    /// keyspace that is not there, credentials the server refused.
    #[error("{0}")]
    Session(#[from] NewSessionError),
    /// A statement that did not work, with the two facts a front end acts on
    /// already read out of the server's error before the rest became a string.
    #[error("{message}")]
    Request {
        message: String,
        /// 1-based, counted in characters, into the text the caller wrote.
        position: Option<u32>,
    },
    /// Somebody pressed Cancel. Carries no server error because there is none —
    /// see the module comment.
    #[error("{0}")]
    Cancelled(&'static str),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl CassandraError {
    /// Reads an `ExecutionError` for the facts a front end needs, resolving any
    /// statement offset against `sent`.
    ///
    /// `sent` must be the text the caller wrote, or `None`. Nothing in this
    /// driver rewrites a user's statement, so the two are the same wherever a
    /// user's statement is what failed; the metadata queries pass `None`
    /// because an offset into `SELECT … FROM system_schema.columns` would put a
    /// caret in text the user never saw.
    fn from_server(error: ExecutionError, sent: Option<&str>) -> Self {
        // The server's own words, where there are any. `RequestAttemptError`'s
        // Display wraps them in "Database returned an error: …, Error message:
        // …", which reads as a driver apologising rather than as a database
        // saying what is wrong with the statement.
        let (message, reason) = match &error {
            ExecutionError::LastAttemptError(RequestAttemptError::DbError(db, reason))
                if !reason.is_empty() =>
            {
                (format!("{db}: {reason}"), Some(reason.as_str()))
            }
            other => (other.to_string(), None),
        };
        CassandraError::Request {
            position: sent.zip(reason).and_then(|(cql, r)| position(r, cql)),
            message,
        }
    }

    /// Where in the statement the server says the trouble is: 1-based, counted
    /// in characters.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            CassandraError::Request { position, .. } => *position,
            _ => None,
        }
    }

    /// Whether this is the Cancel button rather than a fault.
    ///
    /// Decided here and not from anything the server said, which is the reverse
    /// of every other driver in this workspace and follows directly from there
    /// being no server-side cancellation: the statement was stopped on this
    /// side, so this side is the only place that knows.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, CassandraError::Cancelled(_))
    }
}

/// The offset in a Cassandra syntax error, converted to the contract the trait
/// states.
///
/// Cassandra says `line 1:34 no viable alternative at input 'ORDER'`, and the
/// two numbers are a 1-based line and a **0-based column counted in
/// characters**. That last part is the surprise, and it is why there is no byte
/// arithmetic here: the same statement with a CJK identifier in it reports the
/// character position, where ClickHouse in the same situation reports the byte.
/// Checked against Cassandra 5.0.9, which answers 34 for an ASCII statement and
/// 40 for the same statement with six CJK characters ahead of the fault.
///
/// The clamp is for the one case where the counting still disagrees: the
/// server's lexer measures in UTF-16 code units, so a character outside the
/// basic plane counts twice and would push the caret past the end of its line.
/// A caret a little late inside the statement is recoverable; one past the end
/// is not.
fn position(reason: &str, cql: &str) -> Option<u32> {
    let rest = reason.strip_prefix("line ")?;
    let (line, rest) = rest.split_once(':')?;
    let line: usize = line.parse().ok()?;
    let column: usize = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    if line == 0 {
        return None;
    }

    let mut before = 0usize;
    for (at, text) in cql.split('\n').enumerate() {
        let width = text.chars().count();
        if at + 1 == line {
            return u32::try_from(before + column.min(width) + 1).ok();
        }
        before += width + 1; // the newline itself
    }
    None
}

/// A read that has been told to stop, and everything reading under it.
///
/// A generation counter rather than a flag, because a flag would have to be
/// cleared and there is no moment that belongs to: `Driver::cancel` may arrive
/// while one statement is running and another is about to start, and clearing it
/// afterwards would race with whichever went first. A reader records the
/// generation it began in and is cancelled exactly when the counter has moved
/// past it, so a statement started after the button was pressed is not.
///
/// `std::sync::atomic` and `tokio::sync::Notify` rather than a channel: the flag
/// has to be readable from a `Drop` and the wakeup has to reach a fetch that is
/// parked on a socket, and this is the pair that does both without an await on
/// the cancelling side.
#[derive(Debug, Default)]
struct Stop {
    generation: AtomicU64,
    notify: Notify,
}

impl Stop {
    fn now(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn stop(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Resolves once the counter has moved past `since`.
    ///
    /// The waiter is registered before the counter is read, which is the whole
    /// of the correctness here: read first and a `stop` landing in between would
    /// notify nobody and this would park forever.
    async fn stopped(&self, since: u64) {
        loop {
            let notified = self.notify.notified();
            if self.now() != since {
                return;
            }
            notified.await;
        }
    }
}

/// One session against one Cassandra cluster.
///
/// A `Session` and not a connection: the driver crate holds a pool per node and
/// routes each request to a replica for the partition it touches. Nothing in the
/// trait notices — but it is why `transactional` could not become true merely by
/// this driver holding one connection back, the way the MySQL driver's did.
/// There is no statement to hold a transaction open with.
pub struct CassandraSource {
    session: Arc<Session>,
    /// The keyspace the connection string named, and the one unqualified
    /// statements resolve in. Empty where the string named none, which is legal
    /// and means every statement has to qualify its own table.
    keyspace: String,
    /// Shared by every result this session hands out through `query`. A cursor
    /// gets one of its own — the trait says a session cancel does not reach a
    /// cursor, and this is where that is true rather than remembered.
    stop: Arc<Stop>,
}

impl CassandraSource {
    /// Connects to `url`, of the form
    /// `cassandra://[user:password@]host:port/keyspace`.
    ///
    /// The port defaults to 9042 and the keyspace may be left off. The keyspace
    /// is used as written rather than folded to lower case: CQL folds an
    /// unquoted name in a *statement*, and a URL path is not a statement — the
    /// name in it came from `schemas()` or from a console, and both spell it
    /// exactly.
    pub async fn connect(url: &str) -> Result<Self, CassandraError> {
        let parsed =
            url::Url::parse(url).map_err(|e| CassandraError::BadUrl(format!("{url}: {e}")))?;
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| CassandraError::BadUrl(format!("{url}: no host")))?;
        let node = format!("{host}:{}", parsed.port().unwrap_or(DEFAULT_PORT));
        let keyspace = percent_decode(parsed.path().trim_start_matches('/'));

        // Resolved here rather than left to the driver, because the translator
        // below needs the same address and there must be exactly one answer to
        // "where is this database". A client session is opened, used and closed;
        // it is not a server's connection pool outliving a DNS change, so
        // resolving once is the whole of what is needed.
        let endpoint = tokio::net::lookup_host(&node)
            .await
            .map_err(|e| CassandraError::BadUrl(format!("{node}: {e}")))?
            .next()
            .ok_or_else(|| CassandraError::BadUrl(format!("{node}: resolved to no address")))?;

        let mut builder = SessionBuilder::new()
            .known_node_addr(endpoint)
            .address_translator(Arc::new(OneEndpoint(endpoint)));
        if !parsed.username().is_empty() {
            builder = builder.user(
                percent_decode(parsed.username()),
                percent_decode(parsed.password().unwrap_or_default()),
            );
        }
        if !keyspace.is_empty() {
            // Case-sensitive, so the name goes to the server as written. This
            // also makes `build` fail on a keyspace that is not there, which is
            // the round trip that turns "connected" into "connected to
            // something".
            builder = builder.use_keyspace(keyspace.clone(), true);
        }

        Ok(Self {
            session: Arc::new(builder.build().await?),
            keyspace,
            stop: Arc::new(Stop::default()),
        })
    }

    /// The keyspace unqualified names resolve in, or empty where there is none.
    pub fn keyspace(&self) -> &str {
        &self.keyspace
    }

    pub(crate) fn session(&self) -> &Session {
        &self.session
    }

    /// Runs `cql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the columns are known, which costs the first page: CQL has
    /// no `DESCRIBE` of a statement and no way to plan one without running it,
    /// so the result metadata arrives attached to the first page or not at all.
    /// The page is kept rather than thrown away, so the round trip is the one
    /// the caller was going to make anyway.
    pub async fn query(&self, cql: &str, batch_rows: usize) -> Result<Rows, CassandraError> {
        Rows::open(
            Arc::clone(&self.session),
            cql,
            batch_rows,
            Arc::clone(&self.stop),
        )
        .await
    }

    /// Reads `cql` forward, a page at a time.
    ///
    /// The same mechanism as `query`, because Cassandra's paging already is the
    /// thing the trait asks a cursor for. What differs is the canceller: this
    /// result carries a `Stop` of its own, so `Driver::cancel` does not reach it
    /// and closing the front end's Cancel button on one cursor does not stop
    /// the other.
    pub async fn cursor(&self, cql: &str, batch_rows: usize) -> Result<Rows, CassandraError> {
        Rows::open(
            Arc::clone(&self.session),
            cql,
            batch_rows,
            Arc::new(Stop::default()),
        )
        .await
    }

    /// Stops whatever this session is reading.
    ///
    /// Returns immediately and always succeeds, which is the trait's contract —
    /// "success means the request was delivered" — read against a database where
    /// there is no request to deliver. What this actually does is stop reading:
    /// the fetch in flight resolves as a cancelled failure and no further page
    /// is asked for. The coordinator carries on assembling the page it was
    /// asked for and drops it on the floor, and nothing here can prevent that.
    pub async fn cancel(&self) -> Result<(), CassandraError> {
        self.stop.stop();
        Ok(())
    }
}

/// Reaches every node of the cluster at the one address the user gave.
///
/// Without this the driver cannot open a session against a Cassandra behind any
/// kind of address translation, which includes the container this repository
/// tests against. The reason is in `cluster/metadata/fetching.rs`: a node's
/// address is assembled as `SocketAddr::new(untranslated_ip_addr, connect_port)`
/// where `connect_port` is the port of the *contact point*. So a container
/// published as `-p 59042:9042` advertises `172.17.0.2` and the driver dials
/// `172.17.0.2:59042` — an address nothing listens on. The control connection
/// succeeds, `build()` returns, and the first statement fails with "Connection
/// refused". Verified against Cassandra 5.0.9 under Docker: `nc 172.17.0.2 9042`
/// connects, `nc 172.17.0.2 59042` is refused.
///
/// Docker is only the example. A Kubernetes port-forward, an SSH tunnel and a
/// cloud endpoint in front of a private subnet all put the client somewhere the
/// node's own `rpc_address` does not reach — and if it did reach, the user would
/// have typed it.
///
/// The cost is stated rather than hidden: against a multi-node cluster the
/// client *can* reach directly, every request now goes to the one node named
/// instead of being spread, and that node coordinates the rest. That is what
/// connecting through a bastion means, and a client whose heaviest operation is
/// paging a grid is not the thing that load balancing exists for. A driver that
/// worked only when the client and the cluster share a network would be a driver
/// that does not work here.
struct OneEndpoint(SocketAddr);

#[async_trait]
impl AddressTranslator for OneEndpoint {
    async fn translate_address(
        &self,
        _peer: &UntranslatedPeer,
    ) -> Result<SocketAddr, TranslationError> {
        Ok(self.0)
    }
}

/// `%XX` turned back into the byte it stands for.
fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// A result being read forward in pages of the size that was asked for.
///
/// Both a `ResultStream` and a `Cursor`, because against Cassandra they are the
/// same object read the same way — as against ClickHouse, and for the opposite
/// reason. There it was because the database has no cursor and a response body
/// happens to behave like one; here it is because the database has nothing else,
/// and its cursor is good enough that a second mechanism would be a worse copy
/// of it.
pub struct Rows {
    session: Arc<Session>,
    /// The statement, with the page size already on it.
    statement: Statement,
    /// The text the caller wrote, kept so a failure's offset resolves against
    /// what they can see.
    text: String,
    plan: Plan,
    /// Where the coordinator stopped, or `None` once it has reached the end.
    paging: Option<PagingState>,
    /// Rows read out of a page but not yet handed over.
    ///
    /// Cassandra's page size is a request and not a promise — a page comes back
    /// short when the coordinator hits its own row or byte limits first — so a
    /// caller that asked for pages of 50 and was given 47 would see a page
    /// boundary where there is none in the data. The carry is what makes the
    /// page size the caller's number rather than the server's. ClickHouse's
    /// driver carries for exactly the same reason.
    carry: Option<RecordBatch>,
    batch_rows: usize,
    delivered: u64,
    /// Whether the rows handed over are a count of anything.
    ///
    /// False for a statement with no result set. A CQL write answers with an
    /// empty frame and no count at all — not zero, nothing — so an `UPDATE` has
    /// no number to report and says `None` rather than claiming it changed none.
    counted: bool,
    stop: Arc<Stop>,
    /// The generation this result began in; see `Stop`.
    since: u64,
}

impl Rows {
    async fn open(
        session: Arc<Session>,
        cql: &str,
        batch_rows: usize,
        stop: Arc<Stop>,
    ) -> Result<Rows, CassandraError> {
        let batch_rows = batch_rows.max(1);
        // `with_page_size` panics on a non-positive number, so the clamp above
        // is not a nicety.
        let statement =
            Statement::new(cql).with_page_size(i32::try_from(batch_rows).unwrap_or(i32::MAX));
        let since = stop.now();

        let mut rows = Rows {
            session,
            statement,
            text: cql.to_string(),
            plan: Plan::empty(),
            paging: Some(PagingState::start()),
            carry: None,
            batch_rows,
            delivered: 0,
            counted: true,
            stop,
            since,
        };

        // The first page settles the schema, so it is read here rather than
        // left for `next_page`: `query` promises the columns before any row is
        // handed over, and CQL has nowhere else to learn them from.
        let (page, next) = rows.fetch(PagingState::start()).await?;
        rows.paging = next;
        match page {
            Some(page) => {
                rows.plan = Plan::of(page.result.column_specs());
                let batch = rows.plan.batch(&page.rows)?;
                if batch.num_rows() > 0 {
                    rows.carry = Some(batch);
                }
            }
            // No result set: the statement did something and there is nothing
            // to show for it, which is what a write looks like.
            None => {
                rows.paging = None;
                rows.counted = false;
            }
        }
        Ok(rows)
    }

    pub fn schema(&self) -> SchemaRef {
        self.plan.schema()
    }

    /// Rows this statement produced, or `None` until the result has been read to
    /// the end.
    pub fn rows_affected(&self) -> Option<u64> {
        (self.counted && self.paging.is_none() && self.carry.is_none()).then_some(self.delivered)
    }

    /// The next page, or `None` once the result is fully consumed.
    ///
    /// A read that has been stopped stays stopped, which is why the check is
    /// here as well as inside `fetch`: a page already buffered would otherwise
    /// still be handed over after Cancel, and a caller draining a cursor in a
    /// loop would see one more page arrive after they asked for none.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, CassandraError> {
        if self.stop.now() != self.since {
            return Err(CassandraError::Cancelled(STOPPED));
        }
        loop {
            let held = self.carry.as_ref().map_or(0, RecordBatch::num_rows);
            if held >= self.batch_rows {
                return Ok(Some(self.take(self.batch_rows)));
            }
            let Some(state) = self.paging.clone() else {
                return Ok((held > 0).then(|| self.take(held)));
            };

            let (page, next) = self.fetch(state).await?;
            self.paging = next;
            let Some(page) = page else {
                // A paged statement cannot stop being a rows statement halfway
                // through, so this is the end and not a second shape.
                self.paging = None;
                continue;
            };
            let batch = self.plan.batch(&page.rows)?;
            if batch.num_rows() == 0 {
                continue;
            }
            self.carry = match self.carry.take() {
                None => Some(batch),
                Some(held) => Some(arrow::compute::concat_batches(
                    &self.plan.schema(),
                    &[held, batch],
                )?),
            };
        }
    }

    /// One page from the coordinator, or the cancelled failure if the read was
    /// stopped while it was in flight.
    ///
    /// The `select!` is the whole of this driver's cancellation. `biased` so the
    /// stop is looked at first: a Cancel that arrived between two pages must not
    /// have to wait out a third to be noticed. Losing the race the other way is
    /// harmless — the page arrives, and the next call sees the stop.
    async fn fetch(
        &self,
        state: PagingState,
    ) -> Result<(Option<Page>, Option<PagingState>), CassandraError> {
        let stop = Arc::clone(&self.stop);
        let request = self
            .session
            .query_single_page(self.statement.clone(), &[], state);
        tokio::pin!(request);

        let answered = tokio::select! {
            biased;
            () = stop.stopped(self.since) => return Err(CassandraError::Cancelled(STOPPED)),
            answered = &mut request => answered,
        };

        let (result, response) =
            answered.map_err(|e| CassandraError::from_server(e, Some(&self.text)))?;
        let next = match response {
            PagingStateResponse::HasMorePages { state } => Some(state),
            PagingStateResponse::NoMorePages => None,
        };

        match result.into_rows_result() {
            Ok(result) => {
                let rows = result
                    .rows::<Row>()
                    .map_err(|e| CassandraError::Request {
                        message: e.to_string(),
                        position: None,
                    })?
                    .collect::<Result<Vec<Row>, _>>()
                    .map_err(|e| CassandraError::Request {
                        message: e.to_string(),
                        position: None,
                    })?;
                Ok((Some(Page { result, rows }), next))
            }
            Err(IntoRowsResultError::ResultNotRows(_)) => Ok((None, None)),
            Err(e) => Err(CassandraError::Request {
                message: e.to_string(),
                position: None,
            }),
        }
    }

    /// Splits `rows` off the front of the carry.
    fn take(&mut self, rows: usize) -> RecordBatch {
        let held = self.carry.take().expect("take is only called with a carry");
        let page = held.slice(0, rows);
        let rest = held.slice(rows, held.num_rows() - rows);
        self.carry = (rest.num_rows() > 0).then_some(rest);
        self.delivered += page.num_rows() as u64;
        page
    }

    /// A handle for stopping this result from another thread.
    ///
    /// Taken out in advance rather than reached for at cancel time, because by
    /// then the result is borrowed by the fetch that is to be stopped — which is
    /// the whole situation.
    pub fn canceller(&self) -> RowsCancel {
        RowsCancel {
            stop: Arc::clone(&self.stop),
        }
    }

    /// Stops asking for pages and lets go of what is held.
    ///
    /// Optional; dropping does the same. Note what it does **not** do: the
    /// coordinator is not told, because there is nothing to tell it with.
    pub async fn close(&mut self) -> Result<(), CassandraError> {
        self.paging = None;
        self.carry = None;
        Ok(())
    }
}

/// One page, kept beside the result it borrows its column names from.
struct Page {
    result: scylla::response::query_result::QueryRowsResult,
    rows: Vec<Row>,
}

/// Stops the read one result is running.
#[derive(Clone)]
pub struct RowsCancel {
    stop: Arc<Stop>,
}

impl RowsCancel {
    /// Delivered is not interrupted, as everywhere else: a fetch that had
    /// already finished leaves nothing to stop and this still succeeds.
    pub async fn cancel(&self) -> Result<(), CassandraError> {
        self.stop.stop();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no server — it needs the absence of one. Port 1 is reserved and
    /// nothing on a developer machine or a CI runner listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let error = CassandraSource::connect("cassandra://127.0.0.1:1/bench")
            .await
            .err()
            .expect("nothing is listening on port 1");
        let message = error.to_string();
        assert!(
            message.to_lowercase().contains("refused")
                || message.to_lowercase().contains("connect"),
            "the refusal should survive into the message, got: {message}"
        );
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_before_anything_is_sent() {
        let error = CassandraSource::connect("not a url at all")
            .await
            .err()
            .unwrap();
        assert!(matches!(error, CassandraError::BadUrl(_)));
    }

    /// Cassandra counts the offset in characters and so does the trait, which
    /// makes this the one driver in the set with no byte arithmetic in it — and
    /// exactly the reason to pin it, since a change to the server's counting
    /// would otherwise show up as a caret quietly landing in the wrong place.
    ///
    /// Both statements and both numbers are what Cassandra 5.0.9 actually
    /// answered; the assertion is that the caret lands on the `O` of `ORDER` in
    /// each, which is only true for one of the two ways of counting.
    #[test]
    fn a_position_is_counted_in_characters_and_not_bytes() {
        let ascii = "SELECT id FROM system.local WHERE ORDER BY id";
        let reason = "line 1:34 no viable alternative at input 'ORDER'";
        assert_eq!(position(reason, ascii), Some(35));
        assert_eq!(ascii.chars().nth(34), Some('O'));

        // The same statement with a CJK identifier in it. The server says 40,
        // which is the character and not the byte — six of the characters ahead
        // of it are three bytes each, so a byte offset would have been 52.
        let unicode = "SELECT \"漢字漢字漢字\" FROM system.local WHERE ORDER BY id";
        let reason = "line 1:40 no viable alternative at input 'ORDER'";
        assert_eq!(position(reason, unicode), Some(41));
        assert_eq!(unicode.chars().nth(40), Some('O'));
    }

    /// The line is 1-based and the column is 0-based, which is two chances to
    /// be off by one in a statement long enough for it to matter.
    #[test]
    fn an_offset_on_a_later_line_counts_the_lines_before_it() {
        let cql = "SELECT id\nFROM system.local WHERE ORDER BY id";
        let at = position("line 2:24 no viable alternative at input 'ORDER'", cql)
            .expect("an offset") as usize;
        assert_eq!(cql.chars().nth(at - 1), Some('O'));
    }

    /// A failure with nothing to say about where it is says nothing, rather
    /// than pointing at the first character with complete confidence.
    #[test]
    fn a_failure_with_no_offset_in_it_reports_none() {
        assert_eq!(
            position("table no_such_relation_anywhere does not exist", "SELECT 1"),
            None
        );
        assert_eq!(
            position("line 9:0 somewhere else entirely", "SELECT 1"),
            None
        );
        assert_eq!(position("line 0:3 impossible", "SELECT 1"), None);
    }

    /// The counter has to move for the reader that was already running and not
    /// for the one that starts afterwards, or a Cancel would poison the next
    /// statement the user types.
    #[tokio::test]
    async fn a_stop_reaches_the_read_that_was_running_and_not_the_next_one() {
        let stop = Stop::default();
        let running = stop.now();
        stop.stop();
        let started_after = stop.now();

        // Already past its generation: resolves without anything else moving.
        tokio::time::timeout(std::time::Duration::from_secs(1), stop.stopped(running))
            .await
            .expect("a reader from before the stop should be stopped");
        // Not yet: this one has to be waited for, and would hang if it were not.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                stop.stopped(started_after)
            )
            .await
            .is_err(),
            "a reader started after the stop should not be cancelled by it"
        );
    }
}
