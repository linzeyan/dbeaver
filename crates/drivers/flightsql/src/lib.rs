//! Arrow Flight SQL, which is the first database here that speaks Arrow already.
//!
//! Every other driver in this workspace decodes a wire format and builds Arrow
//! arrays by hand — `arrow_map.rs` in the Cassandra driver is 800 lines of
//! exactly that, and ClickHouse's is another 600. There is no `arrow_map.rs`
//! here, and that absence is the finding this driver was written to establish.
//! What follows is what survived contact with a real server.
//!
//! **The format crosses, and so does most of the memory — but what "most" means
//! is decided by the gRPC framing rather than by anything either end chose.** A
//! `DoGet` response is a stream of `FlightData`, each carrying an Arrow IPC
//! record batch in `data_body`. Inside that body the buffers are laid out 8-byte
//! aligned, as the IPC format requires — but the body's *own* start is wherever
//! the gRPC message framing left it, and that is an arbitrary offset into
//! whatever `bytes::Bytes` hyper read the frame into. When it lands on an 8-byte
//! boundary, `read_record_batch` hands back a batch every one of whose buffers is
//! a window into the socket's own bytes. When it does not,
//! `ArrayData::align_buffers` reallocates the fixed-width ones and leaves the
//! rest alone. Reading all 60,175 rows of `lineitem` a whole arrival at a time,
//! 28 of the 30 bodies landed aligned and 9,877,682 bytes of 10,402,010 reached
//! the caller in the memory the socket read — 95%, with the other 5% being the
//! fixed-width columns of the two bodies that did not.
//!
//! That figure is not a promise, because the offset is not stable: read the same
//! table in 100-row pages instead and all 191 bodies land 5 bytes off rather than
//! 28 in 30 landing on zero. The caller's page size is the only thing that
//! changed, so what the offset really follows is how eagerly this side drains the
//! socket — and that is hyper's business and none of this driver's. What is
//! stable is which buffers move. The ones that never do are the values buffers,
//! which is where the bytes actually are — a `Utf8` column's characters need
//! one-byte alignment and are read out of the socket buffer whatever the offset,
//! and only its 4-byte offsets are ever at risk. So the guarantee this driver can
//! make is not "no copy"; it is "no copy that alignment does not force", and the
//! weight of a real result sits on the side that is never forced.
//!
//! `wire_body` and `tests/integration.rs` are what keep that honest. The claim is
//! checkable rather than asserted: the test takes the address range of the gRPC
//! body a batch arrived in and requires every buffer of the delivered batch to be
//! inside it whenever the body was aligned, and requires the variable-width ones
//! to be inside it either way; a second test counts the bytes and requires the
//! in-place side to win. A driver that started re-encoding — or that went back to
//! `arrow_flight::decode::FlightRecordBatchStream` — fails both.
//!
//! **Which is the second finding, and it is about arrow-flight rather than about
//! Flight SQL.** The obvious way to read a `DoGet` is
//! `FlightSqlServiceClient::do_get`, which hands back a `FlightRecordBatchStream`.
//! That decodes through `utils::flight_data_to_arrow_batch`, which takes
//! `&FlightData` and can therefore only do `Buffer::from(data.data_body.as_ref())`
//! — a `&[u8]`, and so a copy of the whole body, for every batch. Measured, that
//! puts every buffer of every batch outside the wire body, every time. The owned
//! `Bytes` is right there in the same struct and `Buffer::from(Bytes)` is free, so
//! this driver decodes the messages itself: about forty lines in `Rows::pull`,
//! against a copy of every byte the server sends.
//!
//! **A cursor and a result stream are the same object, for the third distinct
//! reason in this workspace.** ClickHouse's is because a response body happens to
//! behave like a cursor; Cassandra's is because its paging state already is one.
//! Here it is because a Flight ticket names a result the server has already
//! planned: `GetFlightInfo` prepares and answers in about a millisecond,
//! `DoGet` executes, and the batches are pulled forward one at a time with
//! HTTP/2 flow control providing the backpressure. Page *n* costs what page one
//! costs and nothing is re-read, which is both properties the trait asks a cursor
//! for.
//!
//! **There is no cancellation, and this is the second driver that has to say so.**
//! The protocol has two: `CancelFlightInfo` and the older `CancelQuery`. This
//! server advertises both in `ListActions` — along with `BeginSavepoint`,
//! `SetSessionOptions` and eight more — and implements neither: `CancelFlightInfo`
//! answers `Unimplemented`, and `CancelQuery` is routed to it and answers the
//! same. `ListActions` here is the base class's advertisement rather than the
//! server's capability, so a client that trusted it would put buttons on screen
//! for four features that are not there. So `cancel` stops the read on this side,
//! exactly as the Cassandra driver's does: the in-flight fetch resolves as a
//! failure whose `is_cancelled` is true and no further batch is asked for. One
//! thing it can do that Cassandra's cannot — dropping the `DoGet` stream resets
//! the HTTP/2 stream, so the server is at least told. Whether it acts on that is
//! the server's business, and this one runs its handler synchronously inside
//! DuckDB, so probably not.
//!
//! **Transactions work, and they are not a property of a connection.** Every
//! other driver here answers `transactional` by asking whether its statements
//! share one connection. Flight SQL makes a transaction a *token*:
//! `ActionBeginTransaction` returns a handle, `CommandStatementQuery` carries it,
//! and `ActionEndTransaction` closes it. So this driver holds no connection back
//! and still opens a transaction, reads its own uncommitted write inside it, and
//! rolls it back — all over a stateless pool of HTTP/2 streams. Savepoints are in
//! the protocol too and this driver sends them; this server answers
//! `Unimplemented`, so the three savepoint steps are refused rather than skipped,
//! which is what the trait asks for.
//!
//! **Writes do not need `DoPut`.** `CommandStatementQuery` runs `CREATE TABLE`,
//! `INSERT` and `DELETE` here as happily as a `SELECT` — a write comes back as a
//! one-row result holding the engine's own count, and DDL comes back as a schema
//! message with no batch after it. The protocol's dedicated update path
//! (`CommandStatementUpdate` over `DoPut`) exists and is not used, for a reason
//! worth writing down: on this server its `record_count` is the number of rows in
//! the *result set* rather than the number of rows changed, so a `DELETE` that
//! removed five answers 1. The count in the query path's result is the true one.
//!
//! What this costs is that `rows_affected` counts rows produced, always, because
//! nothing short of parsing the statement would tell this driver which kind it
//! ran — so an `INSERT` of five rows reports 1, being the one row of `Count` the
//! grid is about to show. The trait allows either reading; this driver picks the
//! one it can actually know.
//!
//! **No statement positions.** DuckDB behind this server reports a syntax error
//! by drawing a caret into the message text under a `LINE 1:` heading, the way
//! CockroachDB does, and the gRPC status carries no position field. Parsing the
//! caret back out would be worse here than it is there: what is behind a Flight
//! SQL server is not knowable from the protocol, so the prose being parsed would
//! be DuckDB's today and Spark's or Dremio's tomorrow. The protocol has no
//! position to carry, so none is reported.
//!
//! One limitation stated rather than discovered: this driver speaks plaintext
//! gRPC only. TLS is a tonic feature and a second URL scheme away, and it is left
//! out because there is no TLS Flight SQL server in this repository to test it
//! against, and an untested TLS path is a connection dialog that fails in a way
//! nobody here has seen.

mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::buffer::Buffer;
use arrow::datatypes::{Schema, SchemaRef};
use arrow_flight::sql::client::FlightSqlServiceClient;
use arrow_flight::sql::{ActionBeginSavepointRequest, ActionEndSavepointRequest, ProstMessageExt};
use arrow_flight::{Action, FlightData, FlightInfo, Ticket};
use bytes::Bytes;
use dbconn::TxStep;
use prost::Message;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tonic::Streaming;
use tonic::transport::Channel;

/// The port the Flight SQL reference server listens on, and the one the protocol
/// registered with IANA.
const DEFAULT_PORT: u16 = 31337;

/// What a stopped read says, in one place because it is said in two.
///
/// The second half is the part no other driver but Cassandra's has to write: the
/// stop is on this side, so the server's opinion of it is not asked for.
const STOPPED: &str = "the read was stopped here; this server implements neither CancelFlightInfo \
                       nor CancelQuery, so the statement ends when the reset stream reaches it, \
                       or when it finishes";

/// `ActionEndSavepointRequest.action`, from `FlightSql.proto`.
///
/// Written out because `arrow-flight` generates the enum into a private module
/// and re-exports only the struct around it, so the two numbers are not reachable
/// by name from here. They are protocol constants and cannot move without the
/// protocol moving.
const END_SAVEPOINT_RELEASE: i32 = 1;
const END_SAVEPOINT_ROLLBACK: i32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum FlightSqlError {
    /// Anything the server refused or the transport could not do, reduced to the
    /// message a person reads.
    #[error("{0}")]
    Server(String),
    /// Somebody pressed Cancel. Carries no server error because there is none —
    /// see the module comment.
    #[error("{0}")]
    Cancelled(&'static str),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl FlightSqlError {
    /// Whether this is the Cancel button rather than a fault.
    ///
    /// Decided here and not from anything the server said, which follows directly
    /// from there being no server-side cancellation to ask.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, FlightSqlError::Cancelled(_))
    }
}

/// A `FlightError` reduced to the sentence a person should read.
///
/// Two layers get unwrapped. `FlightError`'s own Display prefixes "Tonic error:",
/// and `tonic::Status`'s prints its whole struct — code, metadata map and all —
/// so a syntax error arrives wrapped in two apologies before the database gets to
/// speak. What is wanted is `Status::message`, which is the server's own words.
fn server_said(error: arrow_flight::error::FlightError) -> FlightSqlError {
    use arrow_flight::error::FlightError;
    let message = match error {
        FlightError::Tonic(status) => status.message().to_string(),
        other => with_causes(&other),
    };
    FlightSqlError::Server(message)
}

/// Renders a failure together with what caused it.
///
/// A connection that never happened carries no gRPC status, and what tonic
/// displays for one names the layer rather than the cause: "transport error"
/// fits every connection failure there is — wrong port, no route, no server, TLS
/// refused — and a connection dialog showing it leaves the user to guess which.
/// The reason is further down the source chain, so the chain is what gets
/// rendered. The ClickHouse driver does the same for the same reason.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        let text = next.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cause = next.source();
    }
    out
}

/// A read that has been told to stop, and everything reading under it.
///
/// A generation counter rather than a flag, because a flag would have to be
/// cleared and there is no moment that belongs to: `cancel` may arrive while one
/// statement is running and another is about to start, and clearing it afterwards
/// would race with whichever went first. A reader records the generation it began
/// in and is cancelled exactly when the counter has moved past it, so a statement
/// started after the button was pressed is not.
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
    /// The waiter is registered before the counter is read, which is the whole of
    /// the correctness here: read first and a `stop` landing in between would
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

/// The transaction this session has open, and the savepoints inside it.
///
/// Handles rather than a connection, which is the shape Flight SQL gives a
/// transaction: `ActionBeginTransaction` answers with an opaque token that every
/// statement then carries. Nothing here is a connection held back, so a
/// `std::sync::Mutex` is enough — it is taken to read or replace a handle and
/// never across an await.
#[derive(Default)]
struct Open {
    transaction: Option<Bytes>,
    /// The server's handle for each savepoint name the caller has opened. The
    /// caller names savepoints and the protocol identifies them by handle, so
    /// somebody has to keep the pairing.
    savepoints: HashMap<String, Bytes>,
}

/// One session against one Flight SQL server.
///
/// A `Channel` and not a connection: tonic multiplexes every call over one
/// HTTP/2 connection, and cloning the client is how a second request happens.
/// That is why `cancel` needs no connection of its own — the situation the
/// PostgreSQL driver opens a second socket for does not arise — and why a
/// transaction can be open while a cursor is being paged.
pub struct FlightSqlSource {
    client: FlightSqlServiceClient<Channel>,
    /// The catalog the connection string named, or empty for every catalog the
    /// server reports. Flight SQL has a catalog level above the schema, as
    /// DuckDB does, and the trait has one string.
    catalog: String,
    open: Mutex<Open>,
    /// Shared by every result this session hands out through `query` and by the
    /// metadata calls. A cursor gets one of its own — the trait says a session
    /// cancel does not reach a cursor, and this is where that is true rather
    /// than remembered.
    stop: Arc<Stop>,
}

impl FlightSqlSource {
    /// Connects to `url`, of the form
    /// `flightsql://[user:password@]host:port/[catalog]`.
    ///
    /// The port defaults to 31337 and the catalog may be left off, which is the
    /// ordinary case: most Flight SQL servers have one and do not name it. Where
    /// it is given it restricts what the navigator shows, which is all a catalog
    /// can do here — the protocol has no way to switch to another.
    pub async fn connect(url: &str) -> Result<Self, FlightSqlError> {
        let parsed =
            url::Url::parse(url).map_err(|e| FlightSqlError::BadUrl(format!("{url}: {e}")))?;
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| FlightSqlError::BadUrl(format!("{url}: no host")))?;
        let port = parsed.port().unwrap_or(DEFAULT_PORT);
        let catalog = percent_decode(parsed.path().trim_start_matches('/'));

        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{host}:{port}"))
            .map_err(|e| FlightSqlError::BadUrl(format!("{url}: {e}")))?;
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| FlightSqlError::Server(with_causes(&e)))?;

        let mut client = FlightSqlServiceClient::new(channel);
        // HTTP basic in, bearer out. The handshake is the one call that carries
        // credentials; everything after it carries the token the server answered
        // with, and a server that answers with none leaves the client
        // unauthenticated, which its own calls will say.
        //
        // Sent even for a connection string with no user in it, because that is
        // the round trip that turns "the port accepted a socket" into "the
        // server answered" — the thing a connection dialog is actually asking.
        client
            .handshake(
                &percent_decode(parsed.username()),
                &percent_decode(parsed.password().unwrap_or_default()),
            )
            .await
            .map_err(server_said)?;

        Ok(Self {
            client,
            catalog,
            open: Mutex::new(Open::default()),
            stop: Arc::new(Stop::default()),
        })
    }

    /// The catalog the navigator is restricted to, or empty where it is not.
    pub fn catalog(&self) -> &str {
        &self.catalog
    }

    /// A client of this session's own, for one call.
    ///
    /// Cloning is how tonic does a second request: the `Channel` inside is an
    /// `Arc` over one multiplexed HTTP/2 connection, so this costs an atomic and
    /// two small clones rather than a socket.
    pub(crate) fn client(&self) -> FlightSqlServiceClient<Channel> {
        self.client.clone()
    }

    /// Runs `sql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the columns are known and before any row is handed over: the
    /// first message of a `DoGet` stream is the schema, so the promise costs
    /// waiting for that message and nothing else. An execution failure can still
    /// arrive later — `GetFlightInfo` only prepares the statement, and `DoGet` is
    /// where it runs — which is exactly the case the trait leaves open.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<Rows, FlightSqlError> {
        let transaction = self.transaction_handle();
        Rows::open(
            self.client(),
            sql,
            batch_rows,
            transaction,
            Arc::clone(&self.stop),
        )
        .await
    }

    /// Reads `sql` forward, a page at a time.
    ///
    /// The same call as `query` but for two things. The result carries a `Stop`
    /// of its own, so `cancel` does not reach it and a front end's Cancel button
    /// on one cursor does not stop the other. And it does not join whatever
    /// transaction the session has open, which is the trait's rule — a cursor is
    /// handed out to be held, and one still being paged after a `Commit` would be
    /// carrying a handle the server has closed.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Rows, FlightSqlError> {
        Rows::open(
            self.client(),
            sql,
            batch_rows,
            None,
            Arc::new(Stop::default()),
        )
        .await
    }

    /// Stops whatever this session is reading.
    ///
    /// Returns immediately and always succeeds, which is the trait's contract —
    /// "success means the request was delivered" — read against a server with
    /// nothing to deliver it to. What this actually does is stop reading: the
    /// fetch in flight resolves as a cancelled failure, the `DoGet` stream is
    /// dropped, and the reset that follows is the only thing the server hears.
    pub async fn cancel(&self) -> Result<(), FlightSqlError> {
        self.stop.stop();
        Ok(())
    }

    /// Takes one step of transaction control.
    ///
    /// Every step is an action the protocol defines, sent as written. A server
    /// that has not implemented one answers `Unimplemented` and that refusal is
    /// what the caller gets, which is the trait's rule — a step this database
    /// does not have is refused rather than skipped. This server implements the
    /// two transaction actions and neither savepoint action.
    pub async fn transaction(&self, step: &TxStep) -> Result<(), FlightSqlError> {
        use arrow_flight::sql::EndTransaction;

        match step {
            TxStep::Begin => {
                let id = self
                    .client()
                    .begin_transaction()
                    .await
                    .map_err(server_said)?;
                let mut open = self.open();
                open.transaction = Some(id);
                open.savepoints.clear();
                Ok(())
            }
            TxStep::Commit | TxStep::Rollback => {
                let id = self
                    .transaction_handle()
                    .ok_or_else(|| FlightSqlError::Server("no transaction is open".to_string()))?;
                let end = match step {
                    TxStep::Commit => EndTransaction::Commit,
                    _ => EndTransaction::Rollback,
                };
                self.client()
                    .end_transaction(id, end)
                    .await
                    .map_err(server_said)?;
                let mut open = self.open();
                open.transaction = None;
                open.savepoints.clear();
                Ok(())
            }
            TxStep::Savepoint(name) => {
                let id = self
                    .transaction_handle()
                    .ok_or_else(|| FlightSqlError::Server("no transaction is open".to_string()))?;
                let handle = self
                    .act(
                        "BeginSavepoint",
                        ActionBeginSavepointRequest {
                            transaction_id: id,
                            name: name.clone(),
                        }
                        .as_any()
                        .encode_to_vec()
                        .into(),
                    )
                    .await?;
                let result: arrow_flight::sql::ActionBeginSavepointResult =
                    arrow_flight::sql::Any::decode(&*handle)
                        .ok()
                        .and_then(|any| any.unpack().ok().flatten())
                        .ok_or_else(|| {
                            FlightSqlError::Server(
                                "the server answered BeginSavepoint with something else"
                                    .to_string(),
                            )
                        })?;
                self.open()
                    .savepoints
                    .insert(name.clone(), result.savepoint_id);
                Ok(())
            }
            TxStep::RollbackTo(name) | TxStep::Release(name) => {
                let handle = self.savepoint_handle(name)?;
                self.act(
                    "EndSavepoint",
                    ActionEndSavepointRequest {
                        savepoint_id: handle,
                        action: if matches!(step, TxStep::RollbackTo(_)) {
                            END_SAVEPOINT_ROLLBACK
                        } else {
                            END_SAVEPOINT_RELEASE
                        },
                    }
                    .as_any()
                    .encode_to_vec()
                    .into(),
                )
                .await?;
                if matches!(step, TxStep::Release(_)) {
                    self.open().savepoints.remove(name);
                }
                Ok(())
            }
        }
    }

    /// Sends one `DoAction` and hands back its first result body.
    ///
    /// The two savepoint actions have no wrapper on `FlightSqlServiceClient`, so
    /// they are built here. An action that answers with nothing is not an error —
    /// `EndSavepoint` is specified to return no message.
    async fn act(&self, name: &str, body: Bytes) -> Result<Bytes, FlightSqlError> {
        let mut results = self
            .client()
            .do_action(Action {
                r#type: name.to_string(),
                body,
            })
            .await
            .map_err(server_said)?;
        let first = results
            .message()
            .await
            .map_err(|status| FlightSqlError::Server(status.message().to_string()))?;
        Ok(first.map(|result| result.body).unwrap_or_default())
    }

    fn open(&self) -> std::sync::MutexGuard<'_, Open> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn transaction_handle(&self) -> Option<Bytes> {
        self.open().transaction.clone()
    }

    fn savepoint_handle(&self, name: &str) -> Result<Bytes, FlightSqlError> {
        self.open().savepoints.get(name).cloned().ok_or_else(|| {
            FlightSqlError::Server(format!(
                "no savepoint called {name} is open on this session"
            ))
        })
    }
}

/// `%XX` turned back into the byte it stands for.
fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// One record batch, and where the gRPC body it was decoded from lives.
///
/// The range is what makes this driver's central claim checkable rather than
/// asserted; see `Rows::wire_body`.
struct Chunk {
    batch: RecordBatch,
    body: Range<usize>,
}

/// A result being read forward, in pages of the size that was asked for.
///
/// Both a `ResultStream` and a `Cursor`, for the reason in the module comment: a
/// Flight ticket names a planned result and `DoGet` reads it forward, which is
/// both of the properties the trait asks a cursor for.
pub struct Rows {
    client: FlightSqlServiceClient<Channel>,
    schema: SchemaRef,
    /// The endpoints not yet read. A `FlightInfo` may name several — that is how
    /// the protocol describes a result spread over partitions — and they are read
    /// one after another so the caller sees one result.
    tickets: VecDeque<Ticket>,
    stream: Option<Streaming<FlightData>>,
    /// Dictionaries seen so far on this stream, by id. A dictionary-encoded
    /// column's batch carries indices only, so the batch cannot be decoded
    /// without them.
    dictionaries: HashMap<i64, arrow::array::ArrayRef>,
    /// Arrivals read out of the stream but not yet handed over, oldest first.
    ///
    /// A queue rather than one buffer, and that is not tidiness. The server's
    /// message size is not the caller's page size — this one sends 2048 rows,
    /// DuckDB's vector, whatever was asked for — so a page that straddles two
    /// arrivals has to be concatenated, and concatenating allocates. Holding the
    /// arrivals apart keeps that copy down to the one page that actually straddles
    /// a boundary: with 100-row pages against 2048-row messages, twenty pages out
    /// of twenty-one are a slice of the socket's own bytes. A single buffer that
    /// was concatenated once stayed concatenated, and the copy spread to every
    /// page after it — measured at 80 pages in 100 before this was a queue.
    carry: VecDeque<Chunk>,
    /// Rows across `carry`, kept rather than summed on every call.
    held: usize,
    /// Where the batch last handed over arrived; see `wire_body`.
    delivered_from: Option<Range<usize>>,
    batch_rows: usize,
    delivered: u64,
    /// Set once every endpoint has been read to its end, which is not the same as
    /// having nothing left to hand over.
    drained: bool,
    stop: Arc<Stop>,
    /// The generation this result began in; see `Stop`.
    since: u64,
}

impl Rows {
    async fn open(
        mut client: FlightSqlServiceClient<Channel>,
        sql: &str,
        batch_rows: usize,
        transaction: Option<Bytes>,
        stop: Arc<Stop>,
    ) -> Result<Rows, FlightSqlError> {
        let info = client
            .execute(sql.to_string(), transaction)
            .await
            .map_err(server_said)?;
        Rows::from_info(client, info, batch_rows, stop).await
    }

    /// A result from a `FlightInfo` somebody else asked for.
    ///
    /// Separate from `open` because the metadata commands produce a `FlightInfo`
    /// the same way a statement does, and reading one is the same work. There is
    /// one decode path in this driver and this is it.
    pub(crate) async fn from_info(
        client: FlightSqlServiceClient<Channel>,
        info: FlightInfo,
        batch_rows: usize,
        stop: Arc<Stop>,
    ) -> Result<Rows, FlightSqlError> {
        let mut tickets = VecDeque::new();
        for endpoint in info.endpoint {
            // An endpoint that names somewhere else is refused rather than read
            // from here. The protocol allows a server to spread a result over
            // other hosts, and following one means opening a connection this
            // session's bearer token may not be good for — so it is said out
            // loud instead of half done. An empty list means "the server you are
            // talking to", which is what every single-node server sends.
            if !endpoint.location.is_empty() {
                return Err(FlightSqlError::Server(format!(
                    "this result is served from {}, and this driver reads endpoints on the \
                     connection it opened",
                    endpoint
                        .location
                        .iter()
                        .map(|l| l.uri.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            if let Some(ticket) = endpoint.ticket {
                tickets.push_back(ticket);
            }
        }

        let since = stop.now();
        let mut rows = Rows {
            client,
            schema: Arc::new(Schema::empty()),
            tickets,
            stream: None,
            dictionaries: HashMap::new(),
            carry: VecDeque::new(),
            held: 0,
            delivered_from: None,
            batch_rows: batch_rows.max(1),
            delivered: 0,
            drained: false,
            stop,
            since,
        };

        // The schema is the first message of the stream, so it is read here
        // rather than left for `next_page`: `query` promises the columns before
        // any row is handed over. A statement with no result set — a `CREATE
        // TABLE` — sends the schema message and then ends, which is why this
        // keeps whatever it read rather than insisting on a batch.
        if let Some(chunk) = rows.pull().await? {
            rows.keep(chunk);
        }
        Ok(rows)
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows this statement produced, or `None` until the result has been read to
    /// the end.
    ///
    /// Rows produced, and not rows changed, whatever the statement was. Flight
    /// SQL's dedicated update path would answer the other question and answers it
    /// wrongly on this server; see the module comment. So an `INSERT` reports the
    /// one row of `Count` it produced rather than the five it wrote, and the
    /// number the user wants is in that row.
    pub fn rows_affected(&self) -> Option<u64> {
        (self.drained && self.carry.is_empty()).then_some(self.delivered)
    }

    /// Where the batch last handed over arrived in memory, or `None` if it was
    /// assembled from more than one arrival.
    ///
    /// Here because the claim this driver exists to make — that a batch reaches
    /// the grid without being re-encoded — is otherwise unfalsifiable prose. The
    /// range is the gRPC body the batch was decoded out of, so a caller can ask
    /// whether the arrays it is holding point into the socket's own bytes;
    /// `tests/integration.rs` does exactly that, and a decode that copied would
    /// put them outside it.
    ///
    /// `None` is the honest answer for a page assembled from two arrivals,
    /// because concatenating is a copy and there is no single body left to point
    /// at. That happens when the caller asks for more rows than the server puts
    /// in a batch, which is the one place in this driver where data is moved.
    pub fn wire_body(&self) -> Option<Range<usize>> {
        self.delivered_from.clone()
    }

    /// The next page, or `None` once the result is fully consumed.
    ///
    /// A read that has been stopped stays stopped, which is why the check is here
    /// as well as inside `pull`: a page already buffered would otherwise still be
    /// handed over after Cancel, and a caller draining a cursor in a loop would
    /// see one more page arrive after they asked for none.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, FlightSqlError> {
        if self.stop.now() != self.since {
            return Err(FlightSqlError::Cancelled(STOPPED));
        }
        loop {
            if self.held >= self.batch_rows {
                return self.take(self.batch_rows).map(Some);
            }
            if self.drained {
                if self.held == 0 {
                    return Ok(None);
                }
                return self.take(self.held).map(Some);
            }
            match self.pull().await? {
                Some(chunk) => self.keep(chunk),
                None => self.drained = true,
            }
        }
    }

    /// Adds one arrival to the queue.
    fn keep(&mut self, chunk: Chunk) {
        if chunk.batch.num_rows() == 0 {
            return;
        }
        self.held += chunk.batch.num_rows();
        self.carry.push_back(chunk);
    }

    /// The next record batch off the wire, or `None` once every endpoint has
    /// ended.
    ///
    /// This is the forty lines the module comment is about. The body arrives as
    /// an owned `bytes::Bytes` and `Buffer::from` takes it whole, so the arrays
    /// built out of it are windows into the bytes hyper read off the socket —
    /// where `arrow_flight::decode` would have copied the lot, because its helper
    /// only ever sees a `&FlightData`.
    ///
    /// The `select!` is this driver's whole cancellation. `biased` so the stop is
    /// looked at first: a Cancel that arrived between two batches must not have
    /// to wait out a third to be noticed. Losing the race the other way is
    /// harmless — the batch arrives, and the next call sees the stop.
    async fn pull(&mut self) -> Result<Option<Chunk>, FlightSqlError> {
        loop {
            if self.stream.is_none() {
                let Some(ticket) = self.tickets.pop_front() else {
                    return Ok(None);
                };
                // The bearer token by hand, because this reaches past
                // `FlightSqlServiceClient::do_get` — which would hand back a
                // decoded batch stream and with it a copy of every body.
                let mut request = tonic::Request::new(ticket);
                if let Some(token) = self.client.token() {
                    let value = format!("Bearer {token}").parse().map_err(|_| {
                        FlightSqlError::Server(
                            "the session token is not a header value".to_string(),
                        )
                    })?;
                    request.metadata_mut().insert("authorization", value);
                }
                self.stream = Some(
                    self.client
                        .inner_mut()
                        .do_get(request)
                        .await
                        .map_err(|status| FlightSqlError::Server(status.message().to_string()))?
                        .into_inner(),
                );
                self.dictionaries.clear();
            }

            let next = {
                let stop = Arc::clone(&self.stop);
                let since = self.since;
                let stream = self.stream.as_mut().expect("a stream was just opened");
                tokio::select! {
                    biased;
                    () = stop.stopped(since) => None,
                    next = stream.message() => Some(next),
                }
            };
            let Some(next) = next else {
                // Dropping the stream resets it, which is the only thing this
                // side can say to a server with no cancel action.
                self.stream = None;
                self.tickets.clear();
                return Err(FlightSqlError::Cancelled(STOPPED));
            };
            let data =
                next.map_err(|status| FlightSqlError::Server(status.message().to_string()))?;
            let Some(data) = data else {
                self.stream = None;
                continue;
            };
            if let Some(chunk) = self.decode(data)? {
                return Ok(Some(chunk));
            }
        }
    }

    /// One `FlightData` turned into a batch, or nothing where it carried schema
    /// or dictionary rather than rows.
    fn decode(&mut self, data: FlightData) -> Result<Option<Chunk>, FlightSqlError> {
        use arrow::error::ArrowError;
        use arrow::ipc::MessageHeader;

        if data.data_header.is_empty() {
            // A message carrying only app metadata, which the protocol allows and
            // which this driver has nothing to do with.
            return Ok(None);
        }
        let message = arrow::ipc::root_as_message(&data.data_header[..])
            .map_err(|e| FlightSqlError::Server(format!("undecodable Flight message: {e}")))?;

        match message.header_type() {
            MessageHeader::Schema => {
                let ipc = message
                    .header_as_schema()
                    .ok_or_else(|| ArrowError::IpcError("a schema that is not one".to_string()))?;
                let schema = Arc::new(arrow::ipc::convert::fb_to_schema(ipc));
                // Every endpoint of one result describes the same columns, and a
                // caller was promised the schema before the first row. A second
                // endpoint that disagreed would silently change the grid under
                // the rows already in it.
                if self.schema.fields().is_empty() {
                    self.schema = schema;
                } else if self.schema != schema {
                    return Err(FlightSqlError::Server(
                        "the endpoints of this result do not agree about its columns".to_string(),
                    ));
                }
                Ok(None)
            }
            MessageHeader::DictionaryBatch => {
                let batch = message.header_as_dictionary_batch().ok_or_else(|| {
                    ArrowError::IpcError("a dictionary batch that is not one".to_string())
                })?;
                arrow::ipc::reader::read_dictionary(
                    &Buffer::from(data.data_body),
                    batch,
                    &self.schema,
                    &mut self.dictionaries,
                    &message.version(),
                )?;
                Ok(None)
            }
            MessageHeader::RecordBatch => {
                let header = message.header_as_record_batch().ok_or_else(|| {
                    ArrowError::IpcError("a record batch that is not one".to_string())
                })?;
                let body = data.data_body;
                let at = body.as_ptr() as usize;
                let range = at..at + body.len();
                let batch = arrow::ipc::reader::read_record_batch(
                    &Buffer::from(body),
                    header,
                    Arc::clone(&self.schema),
                    &self.dictionaries,
                    None,
                    &message.version(),
                )?;
                Ok(Some(Chunk { batch, body: range }))
            }
            other => Err(FlightSqlError::Server(format!(
                "unexpected Flight message: {}",
                other.variant_name().unwrap_or("unknown")
            ))),
        }
    }

    /// Splits `rows` off the front of the queue.
    ///
    /// The whole page comes out of the front arrival wherever it fits there, and
    /// then it is a slice: the page and the remainder go on pointing at the same
    /// buffers, so what the caller holds is still the socket's own bytes. Only a
    /// page that straddles a boundary is concatenated, and it says so by leaving
    /// `wire_body` empty.
    fn take(&mut self, rows: usize) -> Result<RecordBatch, FlightSqlError> {
        self.delivered += rows as u64;
        self.held -= rows;

        if self
            .carry
            .front()
            .is_some_and(|c| c.batch.num_rows() >= rows)
        {
            let front = self.carry.front_mut().expect("just looked");
            let page = front.batch.slice(0, rows);
            self.delivered_from = Some(front.body.clone());
            if front.batch.num_rows() == rows {
                self.carry.pop_front();
            } else {
                front.batch = front.batch.slice(rows, front.batch.num_rows() - rows);
            }
            return Ok(page);
        }

        let mut parts = Vec::new();
        let mut want = rows;
        while want > 0 {
            let front = self
                .carry
                .front_mut()
                .expect("take is only called with enough rows held");
            let taken = want.min(front.batch.num_rows());
            parts.push(front.batch.slice(0, taken));
            if front.batch.num_rows() == taken {
                self.carry.pop_front();
            } else {
                front.batch = front.batch.slice(taken, front.batch.num_rows() - taken);
            }
            want -= taken;
        }
        self.delivered_from = None;
        Ok(arrow::compute::concat_batches(&self.schema, &parts)?)
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

    /// Stops reading and lets go of the stream.
    ///
    /// Optional; dropping does the same. Dropping the `Streaming` resets the
    /// HTTP/2 stream, which is what tells the server nobody is reading — the
    /// nearest thing to a cancel this protocol offers here.
    pub async fn close(&mut self) -> Result<(), FlightSqlError> {
        self.stream = None;
        self.tickets.clear();
        self.carry.clear();
        self.held = 0;
        self.drained = true;
        Ok(())
    }
}

/// Stops the read one result is running.
#[derive(Clone)]
pub struct RowsCancel {
    stop: Arc<Stop>,
}

impl RowsCancel {
    /// Delivered is not interrupted, as everywhere else: a fetch that had already
    /// finished leaves nothing to stop and this still succeeds.
    pub async fn cancel(&self) -> Result<(), FlightSqlError> {
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
        let error = FlightSqlSource::connect("flightsql://127.0.0.1:1/")
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
        let error = FlightSqlSource::connect("not a url at all")
            .await
            .err()
            .unwrap();
        assert!(matches!(error, FlightSqlError::BadUrl(_)));
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

        tokio::time::timeout(std::time::Duration::from_secs(1), stop.stopped(running))
            .await
            .expect("a reader from before the stop should be stopped");
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
