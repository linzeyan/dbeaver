//! BigQuery, read over the Storage Read API so that the rows never stop being
//! Arrow.
//!
//! **No server has ever answered this driver.** There is no BigQuery account
//! behind this repository, no container that serves the API, and no emulator
//! that this project is willing to trust — `goccy/bigquery-emulator` implements
//! a good deal of BigQuery including part of the Storage Read API, and an
//! emulator agreeing with a client is not BigQuery agreeing with it, which
//! matters most for exactly the claim this driver is here to make. Everything
//! below is read from the published protocol: the BigQuery REST v2 reference,
//! `google/cloud/bigquery/storage/v1/*.proto`, and Google's OAuth 2.0
//! service-account documentation. Every other driver in this workspace earned
//! its place against a live server, and the contract suite in
//! `crates/conn/tests/contract.rs` has no subject for this one — deliberately
//! absent rather than faked, because a suite that went green against a mock
//! would be reporting on the mock.
//!
//! What that leaves genuinely unknown, listed rather than implied:
//!
//! - **The protobuf field numbers in `storage.rs`.** They are transcribed by
//!   hand. A wrong one does not fail to compile and does not fail to decode —
//!   prost skips fields it does not recognise — it produces an empty result and
//!   no complaint.
//! - **Whether the OAuth assertion is accepted.** The JWT is built and signed
//!   here and every part of it that can be checked without Google is checked;
//!   whether Google takes it is not one of those parts.
//!   `AUTHENTICATION_UNVERIFIED` in `auth.rs` is the whole of the risk.
//! - **Whether `jobs.query` with `timeoutMs: 0` really answers before the job
//!   finishes**, and whether a `SELECT` always has a destination table to read.
//!   Both are documented; neither has been seen.
//! - **How BigQuery counts the column in `at [1:38]`.** `position` below assumes
//!   characters. If it is bytes, the caret lands in the wrong place on
//!   statements containing non-ASCII and nowhere else.
//! - **Every metadata answer.** `metadata.rs` reads `tables.get`, whose shape is
//!   documented in detail and has never been seen in this repository.
//!
//! What follows is the design, and it is worth reading as a set of choices
//! rather than as a description, because nothing has pushed back on any of them.
//!
//! **The rows come over gRPC and the statement goes over HTTP, and they are two
//! different services.** BigQuery's REST API will run a statement and hand back
//! its rows as JSON, and that is what almost every client does. It is also a
//! transcoding step: the server renders every value to text and the client
//! parses it back. The Storage Read API instead serves a *table* as Arrow IPC
//! over gRPC. So a statement here is three moves — submit a job over REST, wait
//! for it, ask the Storage Read API for the anonymous table BigQuery put the
//! answer in — and the third one is where every row travels. `rest.rs` never
//! calls `jobs.getQueryResults`.
//!
//! **The Arrow bytes are not decoded and re-encoded, and that is checkable
//! without an account.** `storage::decode_batch` is a free function from an
//! owned `bytes::Bytes` to a `RecordBatch` plus the address range of the body it
//! came out of. Its unit tests feed it a message written by Arrow's own IPC
//! writer and require every buffer of the resulting batch to point inside that
//! message — the same measurement the Flight SQL driver makes against a live
//! server, made here over the one step that does not need one. `Rows::wire_body`
//! exposes the same range at the driver's edge, so the day there is a project to
//! point this at, the measurement is a test away rather than a rewrite away.
//!
//! Two things make the no-copy property true, and both are decisions rather than
//! luck. `ArrowRecordBatch::serialized_record_batch` is declared as
//! `bytes::Bytes` rather than `Vec<u8>`, which is what makes prost decode it by
//! splitting tonic's buffer instead of allocating a new one — generated code
//! would have copied every batch and said nothing. And the read session asks for
//! no compression, because decompressing is exactly the transcoding step the
//! design is supposed not to have.
//!
//! **Authentication is a file and a signature, never a browser.** A service
//! account key is turned into an RS256 JWT and exchanged for a bearer token; a
//! developer laptop's Application Default Credentials — which are a refresh
//! token rather than a key — are exchanged the same way with a different grant.
//! Both are a POST. There is no redirect, no consent screen and no loopback
//! listener anywhere in this crate, which is what the phase's exit criterion
//! about native cloud authentication is asking for.
//!
//! **The level above the schema is not flattened into the schema name**, which
//! is where this driver parts company with the DuckDB, Trino and Flight SQL
//! ones. Those three report `catalog.schema` because their extra level is
//! navigable — one connection sees several catalogs. A BigQuery connection names
//! one project and every dataset it can list is in it, so a schema here is a
//! bare dataset id. The project is still written into the browse statement,
//! because that statement is shown to the person about to run it and may be
//! pasted somewhere whose default project is a different one.
//!
//! **There are no transactions**, and the reason is not that BigQuery lacks
//! them. `BEGIN TRANSACTION` … `COMMIT` works inside a *script*, which is one
//! job; this driver submits each statement as its own job, so two statements are
//! two jobs and no transaction can span them. Holding one would mean
//! accumulating statements client-side and submitting them as a script when
//! `COMMIT` arrives, which is a client pretending to be a session. `driver.rs`
//! says no and says why.
//!
//! **Cancel is a real request.** `jobs.cancel` names the job, and BigQuery stops
//! it; the read is stopped on this side at the same time, so a stream already
//! delivering rows does not go on delivering them while the cancel is in flight.
//! That is one better than the Flight SQL driver, which has only the second
//! half.

mod auth;
mod driver;
mod metadata;
mod rest;
mod storage;

use arrow::array::{ArrayRef, RecordBatch};
use arrow::datatypes::SchemaRef;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use auth::Credentials;
use rest::{Api, Job, JobReference};
use storage::{Read, ReadRowsResponse};

/// What BigQuery's audit log shows this connection as.
///
/// Free, and the difference between an expensive query somebody can trace back
/// to a person and one attributed to nothing.
pub(crate) const CLIENT_NAME: &str = "dbclient";

/// Everything a URL may not carry unescaped, which is everything but the
/// unreserved set.
///
/// `NON_ALPHANUMERIC` would do the job and would also escape `-`, `.`, `_` and
/// `~`, which RFC 3986 says never need it. That is legal and it is also how a
/// `grant_type=refresh_token` becomes `grant_type=refresh%5Ftoken` in a log
/// somebody is trying to read. The four exceptions cost one line and keep every
/// value this driver sends recognisable.
pub(crate) const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// How long to wait between asking whether a job has finished.
///
/// Two numbers rather than one, because a job has two regimes. Most statements
/// against a warm cache finish in well under a second, so the first few polls
/// are close together; a scan over a large table takes minutes, and polling it
/// every 50ms would be thousands of requests against an API with a quota. The
/// interval doubles from the first to the second and stays there.
const POLL_FIRST: Duration = Duration::from_millis(50);
const POLL_LONGEST: Duration = Duration::from_millis(2000);

/// What a stopped read says.
///
/// Said in two places, so it lives in one. Unlike the Flight SQL driver's
/// equivalent, this one can promise that the server was told: `jobs.cancel` is a
/// request BigQuery answers, not a stream reset it may or may not notice.
const STOPPED: &str = "the read was stopped here and the job was asked to cancel";

#[derive(Debug, thiserror::Error)]
pub enum BigQueryError {
    /// A statement BigQuery refused, with the two facts a front end acts on
    /// already read out of the answer.
    #[error("{message}")]
    Query {
        message: String,
        /// BigQuery's own word for the fault: `invalidQuery`, `notFound`,
        /// `accessDenied`. Read rather than the HTTP status, for the reason the
        /// ClickHouse driver gives about codes — the status is shared by a dozen
        /// different problems and this is not.
        reason: String,
        /// 1-based, counted in characters, into the text the caller wrote.
        position: Option<u32>,
    },
    /// Anything about the credential: a file that is not one, a key that will
    /// not sign, an endpoint that refused the assertion.
    ///
    /// Its own variant rather than a `Query`, because the two are fixed in
    /// different places — one by editing the statement and the other by
    /// re-running `gcloud auth application-default login`.
    #[error("{0}")]
    Credentials(String),
    /// A request that did not get an answer, or got one that was not
    /// BigQuery's.
    #[error("{0}")]
    Transport(String),
    /// Somebody pressed Cancel.
    #[error("{0}")]
    Cancelled(&'static str),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl BigQueryError {
    /// Whether this is the Cancel button rather than a fault.
    ///
    /// Two ways to be, and both are needed. A read stopped on this side says so
    /// directly. A job that BigQuery stopped because `jobs.cancel` reached it
    /// first comes back as a `Query` whose reason is `stopped`, which is a
    /// failure of the statement and not a fault of the user's — and reporting it
    /// as one would put an error banner on the button they just pressed.
    pub fn is_cancelled(&self) -> bool {
        match self {
            BigQueryError::Cancelled(_) => true,
            BigQueryError::Query { reason, .. } => reason == "stopped",
            _ => false,
        }
    }

    /// Where in the statement BigQuery says the trouble is: 1-based, counted in
    /// characters.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            BigQueryError::Query { position, .. } => *position,
            _ => None,
        }
    }

    /// BigQuery's own word for the fault, where there is one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            BigQueryError::Query { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// The same failure, with its offset resolved against the statement that
    /// produced it.
    ///
    /// Separate from the parsing because most failures have no statement to
    /// resolve against: a metadata request has one this driver wrote and the
    /// user never saw, and putting a caret into text nobody typed is worse than
    /// putting none anywhere. Only the two calls that run a user's statement
    /// pass one.
    fn about(mut self, sql: &str) -> Self {
        if let BigQueryError::Query {
            message, position, ..
        } = &mut self
        {
            *position = statement_position(message, sql);
        }
        self
    }
}

/// Where a GoogleSQL fault is, out of the message that reports it.
///
/// BigQuery has no structured position field anywhere in the REST API: the
/// offset is a suffix on the prose, `Syntax error: Unexpected keyword ORDER at
/// [1:38]`. So this parses prose, which the Flight SQL driver declined to do for
/// its engine and the ClickHouse driver does for its own — and the difference is
/// that here the prose is BigQuery's rather than an unknown engine's. There is
/// exactly one thing behind this protocol.
///
/// Two things are assumed and neither can be checked here. That the suffix is
/// the last `[line:column]` in the message, which is why the search runs from
/// the end — a statement quoted back inside an error message could contain
/// another. And that the column counts **characters**: if BigQuery counts bytes,
/// the caret drifts on statements holding non-ASCII and is exact on every other.
fn statement_position(message: &str, sql: &str) -> Option<u32> {
    let open = message.rfind(" at [")? + " at [".len();
    let close = message[open..].find(']')? + open;
    let (line, column) = message[open..close].split_once(':')?;
    let line: u32 = line.trim().parse().ok()?;
    let column: usize = column.trim().parse().ok()?;
    if line == 0 || column == 0 {
        return None;
    }

    let mut before = 0usize;
    for (index, text) in sql.split('\n').enumerate() {
        let width = text.chars().count();
        if index as u32 + 1 == line {
            // One past the end is where an end-of-input fault points and is a
            // place a front end can draw a caret; two past it is outside the
            // statement.
            return u32::try_from(before + column.min(width + 1)).ok();
        }
        // The newline, which is one character in the text the caller wrote and
        // no character on either line.
        before += width + 1;
    }
    None
}

/// A gRPC failure as the sentence a person should read.
///
/// `tonic::Status`'s own Display prints the whole struct — code, metadata map
/// and all — so a permission problem would arrive wrapped in a debug dump before
/// the service got to speak. What is wanted is `message`, which is the service's
/// own words. The code is kept, because `PERMISSION_DENIED` on the Storage Read
/// API almost always means one specific missing role and the message says which.
pub(crate) fn server_said(status: tonic::Status) -> BigQueryError {
    BigQueryError::Query {
        message: status.message().to_string(),
        reason: format!("{:?}", status.code()),
        position: None,
    }
}

/// Renders a failure together with what caused it.
///
/// A connection that never happened carries no status, and what a transport
/// displays for one names the layer rather than the cause: "transport error"
/// fits every connection failure there is. The reason is further down the source
/// chain, so the chain is what gets rendered. The ClickHouse and Flight SQL
/// drivers do the same for the same reason.
pub(crate) fn with_causes(error: &dyn std::error::Error, what: &str) -> String {
    let mut out = format!("{what}: {error}");
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
/// A generation counter rather than a flag, for the reason the Flight SQL
/// driver's identical type gives: a flag would have to be cleared and there is
/// no moment that belongs to. A reader records the generation it began in and is
/// cancelled exactly when the counter has moved past it, so a statement started
/// after the button was pressed is not.
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

/// The jobs this session has in flight, by reader.
///
/// A `std::sync::Mutex` and not tokio's, because a reader removes its own entry
/// from `Drop`, which cannot await.
type Live = Arc<Mutex<HashMap<u64, JobReference>>>;

/// Puts a job where `BigQuerySource::cancel` can find it, and takes it back out
/// when the reader is dropped.
///
/// A job that has finished is not something to leave in a list: the cancel would
/// name it, do nothing, and cost a round trip proving so.
struct Registration {
    id: u64,
    live: Live,
}

impl Registration {
    fn hold(live: Live, id: u64, job: JobReference) -> Self {
        if let Ok(mut held) = live.lock() {
            held.insert(id, job);
        }
        Self { id, live }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(&self.id);
        }
    }
}

/// One session against one BigQuery project.
///
/// Two clients, because BigQuery is two services: `api` is HTTPS/JSON for jobs
/// and the catalog, `read` is gRPC for rows. Neither is a connection held back —
/// both pool — which is why `cancel` needs nothing of its own and why a cursor
/// can be paged while a statement runs.
pub struct BigQuerySource {
    api: Arc<Api>,
    read: Read,
    /// The project jobs are created in and billed to.
    project: String,
    /// The dataset unqualified names resolve in, or empty where the connection
    /// string named none.
    dataset: String,
    live: Live,
    next: AtomicU64,
    /// Shared by every result this session hands out through `query` and by the
    /// metadata calls. A cursor gets one of its own — the trait says a session
    /// cancel does not reach a cursor.
    stop: Arc<Stop>,
}

impl BigQuerySource {
    /// Connects to `url`, of the form
    /// `bigquery://<project>[/<dataset>][?credentials=<path to a key>]`.
    ///
    /// The project is in the host position, which is the one uncomfortable thing
    /// about this string and is stated rather than hidden: a BigQuery connection
    /// has no host to name — the endpoints are fixed and global — and the
    /// project is the thing that is actually being opened. The dataset is
    /// optional and becomes the default that unqualified names resolve in.
    ///
    /// With no `credentials`, Application Default Credentials are used, and with
    /// no project either the credential's own project is taken — which is what
    /// makes a bare `bigquery://` openable on a laptop that has run `gcloud auth
    /// application-default login`.
    ///
    /// The round trip at the end asks the project for one dataset. A token
    /// exchange alone would prove only that the credential is real and that the
    /// network reaches Google, and would then let a connection succeed against a
    /// project that does not exist or that this credential cannot see — the
    /// failure would surface later, on the first thing the user clicked, as if
    /// the navigator were broken. Listing is also the weakest permission the
    /// navigator needs, so a credential that fails this probe could not have
    /// populated anything anyway.
    pub async fn connect(url: &str) -> Result<Self, BigQueryError> {
        let parsed =
            url::Url::parse(url).map_err(|e| BigQueryError::BadUrl(format!("{url}: {e}")))?;
        let project = percent_decode(parsed.host_str().unwrap_or_default());
        let dataset = percent_decode(parsed.path().trim_matches('/'));
        let key = parsed
            .query_pairs()
            .find(|(name, _)| name == "credentials" || name == "key")
            .map(|(_, value)| value.into_owned());

        let credentials = match key {
            Some(path) => Credentials::parse(
                &std::fs::read_to_string(&path)
                    .map_err(|e| BigQueryError::Credentials(format!("{path}: {e}")))?,
            )?,
            None => {
                let path = auth::adc_path(|name| std::env::var(name).ok()).ok_or_else(|| {
                    BigQueryError::Credentials(
                        "no credentials: name one with ?credentials=<path>, set \
                         GOOGLE_APPLICATION_CREDENTIALS, or run \
                         `gcloud auth application-default login`"
                            .to_string(),
                    )
                })?;
                Credentials::parse(
                    &std::fs::read_to_string(&path).map_err(|e| {
                        BigQueryError::Credentials(format!("{}: {e}", path.display()))
                    })?,
                )?
            }
        };

        let project = match project.is_empty() {
            false => project,
            true => match credentials.project() {
                "" => {
                    return Err(BigQueryError::BadUrl(
                        "this connection names no project and the credentials name none either: \
                         write bigquery://<project>"
                            .to_string(),
                    ));
                }
                named => named.to_string(),
            },
        };

        let api = Arc::new(Api::new(credentials));
        api.get::<serde::de::IgnoredAny>(&rest::project_probe_url(&project))
            .await?;
        let read = Read::lazy()?;

        Ok(Self {
            api,
            read,
            project,
            dataset,
            live: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
            stop: Arc::new(Stop::default()),
        })
    }

    /// The project this connection reads and is billed through.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// The dataset unqualified names resolve in, or empty where there is none.
    pub fn dataset(&self) -> &str {
        &self.dataset
    }

    pub(crate) fn api(&self) -> &Api {
        &self.api
    }

    /// Runs `sql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the columns are known and before any row is handed over.
    /// That costs waiting out the job, which is the honest price of BigQuery's
    /// shape: there is no `DESCRIBE` short of a dry run, and the columns are
    /// settled by the read session, which cannot be created until the job has
    /// somewhere to put its answer.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<Rows, BigQueryError> {
        Rows::open(
            Arc::clone(&self.api),
            self.read.clone(),
            &self.project,
            &self.dataset,
            sql,
            batch_rows,
            Arc::clone(&self.stop),
            Some((
                Arc::clone(&self.live),
                self.next.fetch_add(1, Ordering::Relaxed),
            )),
        )
        .await
    }

    /// Reads `sql` forward, a page at a time.
    ///
    /// The same mechanism as `query`, because a read session already is one: it
    /// names a result BigQuery has finished computing, and reading a stream
    /// forward does not re-read anything and cannot see a later write. Those are
    /// both of the properties the trait asks a cursor for, and there is no second
    /// mechanism here to reach for.
    ///
    /// What differs is that this one carries a `Stop` of its own and is not
    /// registered with the session, so `cancel` does not reach it — the trait
    /// says a session cancel does not touch a cursor.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Rows, BigQueryError> {
        Rows::open(
            Arc::clone(&self.api),
            self.read.clone(),
            &self.project,
            &self.dataset,
            sql,
            batch_rows,
            Arc::new(Stop::default()),
            None,
        )
        .await
    }

    /// Asks BigQuery to abandon whatever this session is running.
    ///
    /// Two halves, because a statement is in one of two states and they are
    /// stopped differently. A job still running is stopped by `jobs.cancel`,
    /// which is a request BigQuery answers. A read already streaming rows is
    /// stopped here, by moving the generation counter — the job has finished by
    /// then and there is nothing left to cancel.
    ///
    /// Best-effort, as the trait says. A session with nothing running sends no
    /// request at all.
    pub async fn cancel(&self) -> Result<(), BigQueryError> {
        self.stop.stop();
        let jobs: Vec<JobReference> = match self.live.lock() {
            Ok(live) => live.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        for job in jobs {
            cancel_job(&self.api, &job).await?;
        }
        Ok(())
    }
}

/// Asks BigQuery to stop one job.
///
/// A `POST` with an empty body, whose answer this driver reads only for its
/// status. Naming a job that has already finished succeeds and does nothing,
/// which is what makes cancelling an idle session harmless.
async fn cancel_job(api: &Api, job: &JobReference) -> Result<(), BigQueryError> {
    let url = rest::cancel_url(&job.project_id, &job.job_id, &job.location);
    let _: serde_json::Value = api.post(&url, serde_json::json!({})).await?;
    Ok(())
}

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
/// Both a `ResultStream` and a `Cursor`; see `BigQuerySource::cursor`.
pub struct Rows {
    api: Arc<Api>,
    read: Read,
    schema: SchemaRef,
    /// The streams of the session not yet read. Always at most one, because the
    /// session asks for one; a `VecDeque` because the API's shape is many and
    /// pretending otherwise would make a second stream silently unread.
    streams: VecDeque<String>,
    stream: Option<tonic::Streaming<ReadRowsResponse>>,
    /// Dictionaries seen so far on this stream, by id.
    dictionaries: HashMap<i64, ArrayRef>,
    /// Arrivals read out of the stream but not yet handed over, oldest first.
    ///
    /// A queue rather than one buffer, for the reason the Flight SQL driver
    /// gives: the service's batch size is not the caller's page size, so a page
    /// that straddles two arrivals has to be concatenated, and concatenating
    /// allocates. Holding the arrivals apart keeps that copy down to the pages
    /// that actually straddle a boundary instead of spreading it to every page
    /// after the first.
    carry: VecDeque<Chunk>,
    /// Rows across `carry`, kept rather than summed on every call.
    held: usize,
    /// Where the batch last handed over arrived; see `wire_body`.
    delivered_from: Option<Range<usize>>,
    batch_rows: usize,
    delivered: u64,
    /// Rows a DML statement changed, which BigQuery counts for itself.
    dml_rows: Option<u64>,
    /// Set once every stream has been read to its end, which is not the same as
    /// having nothing left to hand over.
    drained: bool,
    stop: Arc<Stop>,
    /// The generation this result began in; see `Stop`.
    since: u64,
    job: JobReference,
    _registration: Option<Registration>,
}

impl Rows {
    #[allow(clippy::too_many_arguments)]
    async fn open(
        api: Arc<Api>,
        read: Read,
        project: &str,
        dataset: &str,
        sql: &str,
        batch_rows: usize,
        stop: Arc<Stop>,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, BigQueryError> {
        let since = stop.now();
        let mut request = serde_json::json!({
            "query": sql,
            // GoogleSQL and not the 2011 dialect. The default is still legacy
            // SQL on this endpoint, which would make every statement in the
            // editor a different language from the one the completion offers.
            "useLegacySql": false,
            // Do not attach rows to the answer, and do not wait for them: see
            // the header of `rest.rs`. Both are what turn this call into "create
            // this job and tell me its id".
            "maxResults": 0,
            "timeoutMs": 0,
        });
        if !dataset.is_empty() {
            request["defaultDataset"] =
                serde_json::json!({ "projectId": project, "datasetId": dataset });
        }

        let created: Job = api
            .post(&rest::queries_url(project), request)
            .await
            .map_err(|e| e.about(sql))?;
        let job = created.job_reference.clone();
        // Held from here rather than from the first page, so that a statement
        // cancelled while it is still `PENDING` is one `cancel` can name.
        let registration = register.map(|(live, id)| Registration::hold(live, id, job.clone()));

        let finished = wait_for(&api, &job, &stop, since)
            .await
            .map_err(|e| e.about(sql))?;
        if let Some(failure) = finished.status.error_result {
            return Err(BigQueryError::Query {
                message: failure.message,
                reason: failure.reason,
                position: None,
            }
            .about(sql));
        }

        let statistics = finished.statistics.query.unwrap_or_default();
        let dml_rows = rest::count(&statistics.num_dml_affected_rows);
        let destination = finished
            .configuration
            .query
            .and_then(|query| query.destination_table);

        let mut rows = Rows {
            api,
            read,
            schema: storage::empty_schema(),
            streams: VecDeque::new(),
            stream: None,
            dictionaries: HashMap::new(),
            carry: VecDeque::new(),
            held: 0,
            delivered_from: None,
            batch_rows: batch_rows.max(1),
            delivered: 0,
            dml_rows,
            // A statement with no destination table produced no rows, and there
            // is nothing to open a session on. `CREATE TABLE`, `INSERT` and
            // `CALL` all land here.
            drained: destination.is_none(),
            stop,
            since,
            job,
            _registration: registration,
        };

        if let Some(table) = destination {
            let token = rows.api.token().await?;
            let session = rows
                .read
                .create_session(&token, project, &table.resource())
                .await
                .map_err(|e| e.about(sql))?;
            if let Some(schema) = &session.arrow_schema {
                rows.schema = storage::read_schema(&schema.serialized_schema)?;
            }
            rows.streams = session.streams.into_iter().map(|s| s.name).collect();
            // A session with no streams is an empty result, not a fault:
            // BigQuery hands out no stream for a table with no rows in it.
            rows.drained = rows.streams.is_empty();
        }
        Ok(rows)
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows this statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Two answers, because BigQuery gives two and they are about different
    /// statements. A write reports what it changed, which the job's own
    /// statistics carry as `numDmlAffectedRows` — a better number than the Flight
    /// SQL driver can offer and the same one Trino's gives. A read reports what
    /// it produced, counted here.
    pub fn rows_affected(&self) -> Option<u64> {
        if !self.drained || !self.carry.is_empty() {
            return None;
        }
        Some(self.dml_rows.unwrap_or(self.delivered))
    }

    /// Where the batch last handed over arrived in memory, or `None` if it was
    /// assembled from more than one arrival.
    ///
    /// Here because the claim this driver exists to make — that a batch reaches
    /// the grid without being decoded and re-encoded — is otherwise
    /// unfalsifiable prose. The range is the Arrow IPC body the batch was
    /// decoded out of, so a caller can ask whether the arrays it is holding
    /// point into the bytes gRPC read; `storage::tests` asks exactly that of the
    /// decode step, and the day there is a project to point this at, the same
    /// question can be asked of a whole result.
    ///
    /// `None` is the honest answer for a page assembled from two arrivals,
    /// because concatenating is a copy and there is no single body left to point
    /// at.
    pub fn wire_body(&self) -> Option<Range<usize>> {
        self.delivered_from.clone()
    }

    /// The next page, or `None` once the result is fully consumed.
    ///
    /// A read that has been stopped stays stopped, which is why the check is
    /// here as well as inside `pull`: a page already buffered would otherwise
    /// still be handed over after Cancel.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, BigQueryError> {
        if self.stop.now() != self.since {
            return Err(BigQueryError::Cancelled(STOPPED));
        }
        loop {
            if self.held >= self.batch_rows {
                return self.take(self.batch_rows).map(Some);
            }
            if self.drained {
                if self.held == 0 {
                    return Ok(None);
                }
                let held = self.held;
                return self.take(held).map(Some);
            }
            match self.pull().await? {
                Some(chunk) => self.keep(chunk),
                None => self.drained = true,
            }
        }
    }

    fn keep(&mut self, chunk: Chunk) {
        if chunk.batch.num_rows() == 0 {
            return;
        }
        self.held += chunk.batch.num_rows();
        self.carry.push_back(chunk);
    }

    /// The next record batch off the wire, or `None` once every stream has
    /// ended.
    ///
    /// The `select!` is this driver's half of cancellation — the other half is
    /// `jobs.cancel`, which has already happened by the time a read is
    /// streaming. `biased` so the stop is looked at first: a Cancel that arrived
    /// between two batches must not have to wait out a third to be noticed.
    /// Losing the race the other way is harmless — the batch arrives, and the
    /// next call sees the stop.
    async fn pull(&mut self) -> Result<Option<Chunk>, BigQueryError> {
        loop {
            if self.stream.is_none() {
                let Some(name) = self.streams.pop_front() else {
                    return Ok(None);
                };
                let token = self.api.token().await?;
                self.stream = Some(self.read.read_rows(&token, &name).await?);
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
                // Dropping the stream resets it, which tells the service nobody
                // is reading. The job itself has already finished by this point,
                // so there is nothing else left to stop.
                self.stream = None;
                self.streams.clear();
                return Err(BigQueryError::Cancelled(STOPPED));
            };
            let Some(response) = next.map_err(server_said)? else {
                self.stream = None;
                continue;
            };
            let Some(batch) = response.arrow_record_batch else {
                // A response carrying only progress or throttling, which the
                // protocol allows and which this driver has nothing to do with.
                continue;
            };
            if let Some((batch, body)) = storage::decode_batch(
                &self.schema,
                &mut self.dictionaries,
                batch.serialized_record_batch,
            )? {
                return Ok(Some(Chunk { batch, body }));
            }
        }
    }

    /// Splits `rows` off the front of the queue.
    ///
    /// The whole page comes out of the front arrival wherever it fits there, and
    /// then it is a slice: the page and the remainder go on pointing at the same
    /// buffers, so what the caller holds is still the bytes gRPC read. Only a
    /// page that straddles a boundary is concatenated, and it says so by leaving
    /// `wire_body` empty.
    fn take(&mut self, rows: usize) -> Result<RecordBatch, BigQueryError> {
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
            api: Arc::clone(&self.api),
            stop: Arc::clone(&self.stop),
            job: self.job.clone(),
        }
    }

    /// Stops reading and lets go of the stream.
    ///
    /// Optional; dropping does the same. Note what neither does: BigQuery is not
    /// told. The job has finished by the time rows are streaming, and the read
    /// session expires on its own — the service gives one a lifetime measured in
    /// hours and charges nothing for one nobody reads.
    pub async fn close(&mut self) -> Result<(), BigQueryError> {
        self.stream = None;
        self.streams.clear();
        self.carry.clear();
        self.held = 0;
        self.drained = true;
        Ok(())
    }
}

/// Stops the statement one result is running.
#[derive(Clone)]
pub struct RowsCancel {
    api: Arc<Api>,
    stop: Arc<Stop>,
    job: JobReference,
}

impl RowsCancel {
    /// Delivered is not interrupted, as everywhere else: a statement that had
    /// already finished leaves nothing to stop and this still succeeds.
    ///
    /// Both halves, in this order. The stop goes first because it cannot fail
    /// and takes no time, so a reader parked on a batch is released whatever
    /// happens to the request that follows.
    pub async fn cancel(&self) -> Result<(), BigQueryError> {
        self.stop.stop();
        if self.job.job_id.is_empty() {
            return Ok(());
        }
        cancel_job(&self.api, &self.job).await
    }
}

/// Waits for a job to reach `DONE`, or for somebody to press Cancel.
///
/// The polling interval is the interesting part and it is argued at `POLL_FIRST`.
/// What is not negotiable is that the wait happens here rather than inside one
/// long HTTP request: a request this side is parked on is a request the Cancel
/// button cannot reach, and a ten-minute query would then be a client that has
/// apparently hung.
async fn wait_for(
    api: &Api,
    job: &JobReference,
    stop: &Stop,
    since: u64,
) -> Result<Job, BigQueryError> {
    let url = rest::job_url(&job.project_id, &job.job_id, &job.location);
    let mut interval = POLL_FIRST;
    loop {
        let state: Job = api.get(&url).await?;
        if state.status.state == "DONE" {
            return Ok(state);
        }
        tokio::select! {
            biased;
            () = stop.stopped(since) => return Err(BigQueryError::Cancelled(STOPPED)),
            () = tokio::time::sleep(interval) => {}
        }
        interval = (interval * 2).min(POLL_LONGEST);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL that is not one is refused before anything is sent — before, in
    /// particular, a credentials file is looked for, which is what makes this
    /// test need nothing on disk.
    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_before_anything_is_sent() {
        let error = BigQuerySource::connect("not a url at all")
            .await
            .err()
            .expect("that is not a URL");
        assert!(matches!(error, BigQueryError::BadUrl(_)), "{error}");
    }

    /// A credentials file that is not there is named, rather than reported as a
    /// connection failure — they are different problems with different fixes.
    #[tokio::test]
    async fn a_key_file_that_is_not_there_says_so() {
        let error = BigQuerySource::connect(
            "bigquery://example-project/tables?credentials=/nonexistent/key.json",
        )
        .await
        .err()
        .expect("that file is not there");
        let message = error.to_string();
        assert!(matches!(error, BigQueryError::Credentials(_)), "{message}");
        assert!(message.contains("/nonexistent/key.json"), "{message}");
    }

    /// The caret, out of the only place BigQuery puts one: the end of the
    /// message.
    ///
    /// The assertion is that the offset lands on the `O` of `ORDER`, which is
    /// true of exactly one reading of `[1:38]`.
    #[test]
    fn a_position_is_read_off_the_end_of_the_message() {
        let sql = "SELECT id FROM `p.d.orders` WHERE ORDER BY id";
        let message = "Syntax error: Unexpected keyword ORDER at [1:35]";
        let at = statement_position(message, sql).expect("an offset") as usize;
        assert_eq!(sql.chars().nth(at - 1), Some('O'));
    }

    /// The column is counted in characters here, which is an assumption and is
    /// the one this test pins so that a day with a real project turns it into a
    /// failing test rather than a caret quietly landing in the wrong place.
    #[test]
    fn a_position_is_counted_in_characters_and_not_bytes() {
        // Six CJK characters ahead of the fault. Three bytes each, so a byte
        // offset for the same character would have been 42 rather than 30.
        let sql = "SELECT `漢字漢字漢字` FROM t WHERE ORDER BY id";
        let at = statement_position("Syntax error: … at [1:30]", sql).expect("an offset") as usize;
        assert_eq!(sql.chars().nth(at - 1), Some('O'));
    }

    /// A later line counts the lines before it, which is two chances to be off
    /// by one.
    #[test]
    fn an_offset_on_a_later_line_counts_the_lines_before_it() {
        let sql = "SELECT id FROM t\nWHERE ORDER BY id";
        let at = statement_position("Syntax error at [2:7]", sql).expect("an offset") as usize;
        assert_eq!(sql.chars().nth(at - 1), Some('O'));
    }

    /// A message with no offset in it, and one whose offset cannot be this
    /// statement.
    #[test]
    fn a_message_with_no_offset_produces_none() {
        assert_eq!(
            statement_position("Not found: Table p:d.t", "SELECT 1"),
            None
        );
        assert_eq!(statement_position("… at [9:1]", "SELECT 1"), None);
        assert_eq!(statement_position("… at [1:0]", "SELECT 1"), None);
        assert_eq!(statement_position("… at [x:y]", "SELECT 1"), None);
        // One past the end is where an end-of-input fault points; further than
        // that is clamped to it rather than pointing outside the statement.
        assert_eq!(statement_position("… at [1:9]", "SELECT 1"), Some(9));
        assert_eq!(statement_position("… at [1:40]", "SELECT 1"), Some(9));
    }

    /// The statement quoted back inside its own error message is why the search
    /// runs from the end.
    #[test]
    fn the_last_offset_in_a_message_is_the_one_that_is_read() {
        let sql = "SELECT 1";
        let message = "Invalid value at [1:1]: the statement at [1:8] is not one";
        assert_eq!(statement_position(message, sql), Some(8));
    }

    /// A job BigQuery stopped because Cancel reached it is the button working,
    /// not a fault — and the front end hides one and shows the other.
    #[test]
    fn a_job_bigquery_stopped_is_reported_as_cancelled() {
        let stopped = BigQueryError::Query {
            message: "Job was cancelled".to_string(),
            reason: "stopped".to_string(),
            position: None,
        };
        assert!(stopped.is_cancelled());
        let broken = BigQueryError::Query {
            message: "Syntax error".to_string(),
            reason: "invalidQuery".to_string(),
            position: None,
        };
        assert!(!broken.is_cancelled());
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

        tokio::time::timeout(Duration::from_secs(1), stop.stopped(running))
            .await
            .expect("a reader from before the stop should be stopped");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stop.stopped(started_after))
                .await
                .is_err(),
            "a reader started after the stop should not be cancelled by it"
        );
    }
}
