//! Amazon Athena, over its own API rather than through the AWS SDK.
//!
//! **No server has ever answered this driver.** There is no AWS account behind
//! this repository, no container that serves the Athena API, and nothing this
//! project would trust as a substitute for one. Everything below is read from
//! the published protocol: the Athena API reference, the Signature Version 4
//! specification, and Presto's type documentation, which is what Athena's engine
//! is. Every other driver in this workspace earned its place against a live
//! server, and the contract suite in `crates/conn/tests/contract.rs` has no
//! subject for this one — deliberately absent rather than faked, because a suite
//! that went green against a mock would be reporting on the mock.
//!
//! What that leaves genuinely unknown, listed rather than implied:
//!
//! - **Whether `GetQueryExecution` classifies statements the way
//!   `Rows::open` assumes.** The rule that drops the repeated header row is
//!   `StatementType == "DML"` *and* the row being the column names; both halves
//!   are argued at `arrow_map::Plan::is_header`, and only an account settles
//!   whether the first half is the right predicate.
//! - **How a `varbinary` is rendered.** Every Athena value arrives as text and
//!   nothing this was written from says which encoding bytes get, so they are
//!   handed over as the text they arrived as.
//! - **Whether a `timestamp` ever has more than six fractional digits.** There
//!   is no precision in `ColumnInfo` to read, and a value with more is refused
//!   rather than truncated.
//! - **Whether the engine counts an error's column in characters.** `position`
//!   below assumes it does, because Trino — the same engine, measured in this
//!   repository against a live server — does. Athena's fork could differ.
//! - **Every metadata answer.** `metadata.rs` reads the three catalog actions,
//!   whose shapes are documented and have never been seen here.
//!
//! What is *not* unknown is the signature. SigV4 is a pure function and AWS
//! publishes worked examples of every step of it; `sigv4.rs` computes them and
//! its tests are those examples. That is the one part of this driver that is as
//! well established as it would be with an account.
//!
//! What follows is the design, and it is worth reading as a set of choices
//! rather than as a description, because nothing has pushed back on any of them.
//!
//! **Four calls run a statement and they are all the same POST.** Athena has no
//! session, no connection and no cursor: `StartQueryExecution` hands back an id,
//! `GetQueryExecution` says whether that id has finished,
//! `GetQueryResults` pages through what it produced, and
//! `StopQueryExecution` stops it. The id is the whole of the state, which is
//! what makes cancel an ordinary second request rather than a contended one —
//! the situation the PostgreSQL driver opens a second socket for does not arise,
//! for the third time in this workspace and for the third distinct reason.
//!
//! **A cursor and a query are the same call**, as in ClickHouse and Trino. A
//! query execution is finished before the first row is read — the rows are a
//! file Athena has already written to S3 — so paging through it re-reads
//! nothing and cannot see a later write. Those are exactly the two properties
//! `Driver::cursor` asks for, and `LIMIT`/`OFFSET`, which the trait exists
//! instead of, is not reached for.
//!
//! **The page size is the caller's, bounded by the service's.**
//! `GetQueryResults` takes at most 1000 rows per call, so a caller asking for
//! more gets several requests joined together, and one asking for fewer gets a
//! page sliced out of what arrived. That is the same carry the Trino driver
//! needs and for the same reason: the service's page size is not the grid's.
//!
//! **The first row of the first page is the column headers, sometimes.** It is
//! Athena's best-known quirk and the awkward half of this driver. The rule for
//! dropping it is two conditions rather than one, and the pairing is chosen so
//! that the way it can fail is visible rather than silent;
//! `arrow_map::Plan::is_header` argues it at length.
//!
//! **Every value arrives as text and the column's declared type is the only
//! thing that says what it is.** `arrow_map.rs` is therefore a parser where the
//! Trino driver's equivalent is a reader of JSON types.
//!
//! **The navigator never runs a query.** `SHOW DATABASES` and `DESCRIBE` both
//! work in Athena and both are *query executions* — scanned bytes, a result file
//! written to S3, a line on the bill. `ListDatabases`, `ListTableMetadata` and
//! `GetTableMetadata` answer the same questions as ordinary API calls that cost
//! nothing. A client that expanded a tree by running SQL would charge somebody
//! for opening it.
//!
//! **There are no transactions.** Athena has no `BEGIN`; an Iceberg table gives
//! one statement atomicity and there is nothing that spans two. `driver.rs` says
//! so and refuses the steps rather than skipping them.

mod arrow_map;
mod credentials;
mod driver;
mod metadata;
mod sigv4;
mod wire;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow_map::Plan;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use credentials::Keys;
use sigv4::Signer;
use wire::{Execution, Results, Row, Started, Wire};

/// The data catalog a connection string that names none is given.
///
/// Every AWS account has this one and it is the Glue Data Catalog; the others a
/// connection could name are federated catalogs backed by a Lambda function,
/// which are opt-in and rare.
const DEFAULT_CATALOG: &str = "AwsDataCatalog";

/// The workgroup a connection string that names none is given.
///
/// Every account has `primary`, created with the account and not deletable.
const DEFAULT_WORKGROUP: &str = "primary";

/// The most rows `GetQueryResults` will return in one call.
///
/// The service's ceiling and not a choice. It is what makes `Rows` need a carry
/// even for a caller asking for a modest page: a grid asking for 2000 rows is
/// two requests.
const PAGE_CEILING: usize = 1000;

/// How long to wait between asking whether a query has finished.
///
/// Two numbers rather than one, for the reason the BigQuery driver gives about
/// the same shape of wait: a statement against a small table finishes in a
/// second or two and a scan over a partitioned lake takes minutes, and one
/// interval cannot serve both without either wasting requests or wasting time.
const POLL_FIRST: Duration = Duration::from_millis(100);
const POLL_LONGEST: Duration = Duration::from_millis(2000);

/// What a stopped read says.
const STOPPED: &str = "the read was stopped here and the query was asked to stop";

#[derive(Debug, thiserror::Error)]
pub enum AthenaError {
    /// A statement Athena refused, with the two facts a front end acts on
    /// already read out of the answer.
    #[error("{message}")]
    Query {
        message: String,
        /// The exception's shape name — `InvalidRequestException`,
        /// `TooManyRequestsException` — or the engine's own error category for
        /// a statement that failed while running. The closest thing Athena has
        /// to a code.
        kind: String,
        /// 1-based, counted in characters, into the text the caller wrote.
        position: Option<u32>,
    },
    /// Anything about the credentials: none found, half a key, a profile that is
    /// not there.
    ///
    /// Its own variant rather than a `Query`, because the two are fixed in
    /// different places — one by editing the statement and the other by editing
    /// `~/.aws/credentials`.
    #[error("{0}")]
    Credentials(String),
    /// A request that did not get an answer, or got one that was not Athena's.
    #[error("{0}")]
    Transport(String),
    /// Somebody pressed Cancel — here, or in the AWS console.
    #[error("{0}")]
    Cancelled(&'static str),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl AthenaError {
    /// Whether this is the Cancel button rather than a fault.
    ///
    /// One arm, because a query Athena reports as `CANCELLED` is turned into
    /// this variant where it is read rather than carried as a `Query` and
    /// classified later. That covers the case somebody else pressed the button:
    /// a query stopped from the AWS console reaches this driver as `CANCELLED`
    /// too, and it is no more a fault than one stopped here.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, AthenaError::Cancelled(_))
    }

    /// Where in the statement Athena says the trouble is: 1-based, counted in
    /// characters.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            AthenaError::Query { position, .. } => *position,
            _ => None,
        }
    }

    /// The exception shape, where there is one.
    pub fn kind(&self) -> Option<&str> {
        match self {
            AthenaError::Query { kind, .. } => Some(kind),
            _ => None,
        }
    }

    /// The same failure, with its offset resolved against the statement that
    /// produced it.
    ///
    /// Separate from the parsing because most failures have no statement to
    /// resolve against: a metadata call has none at all, and putting a caret
    /// into text nobody typed is worse than putting none anywhere.
    fn about(mut self, sql: &str) -> Self {
        if let AthenaError::Query {
            message, position, ..
        } = &mut self
        {
            *position = statement_position(message, sql);
        }
        self
    }
}

/// Where an engine fault is, out of the message that reports it.
///
/// Athena has no structured position field: the offset is a prefix on the
/// prose, `SYNTAX_ERROR: line 1:35: mismatched input 'ORDER'`. So this parses
/// prose, which the Flight SQL driver declined to do for its engine — and the
/// difference is that what is behind Athena is known. It is Presto, and the
/// Trino driver in this workspace has the same engine's numbers measured
/// against a live server: **1-based line, 1-based column, counted in code
/// points**. That measurement is the reason `position` below has no byte
/// arithmetic in it; whether Athena's fork counts the same way is the part no
/// test here can settle.
///
/// The *first* `line L:C` in the message and not the last, which is the
/// opposite of the BigQuery driver's choice about its own messages. Athena puts
/// the offset in front, and what follows can quote the statement — including,
/// for a `mismatched input`, a list of what was expected.
fn statement_position(message: &str, sql: &str) -> Option<u32> {
    let (line, column) = message.match_indices("line ").find_map(|(at, _)| {
        let rest = &message[at + "line ".len()..];
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if rest.as_bytes().get(digits) != Some(&b':') {
            return None;
        }
        let line: u32 = rest[..digits].parse().ok()?;
        let after = &rest[digits + 1..];
        let width = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        let column: usize = after[..width].parse().ok()?;
        Some((line, column))
    })?;
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

/// Renders a failure together with what caused it.
///
/// A connection that never happened carries no HTTP status, and what hyper
/// displays for one names the layer rather than the cause. The reason is further
/// down the source chain, so the chain is what gets rendered — as in the
/// ClickHouse, Flight SQL and BigQuery drivers.
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
/// The same generation counter the Flight SQL and BigQuery drivers use, for the
/// same reason: a flag would have to be cleared and there is no moment that
/// belongs to.
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

/// The query executions this session has in flight, by reader.
///
/// A `std::sync::Mutex` and not tokio's, because a reader removes its own entry
/// from `Drop`, which cannot await.
type Live = Arc<Mutex<HashMap<u64, String>>>;

/// Puts an execution id where `AthenaSource::cancel` can find it, and takes it
/// back out when the reader is dropped.
struct Registration {
    id: u64,
    live: Live,
}

impl Registration {
    fn hold(live: Live, id: u64, execution: String) -> Self {
        if let Ok(mut held) = live.lock() {
            held.insert(id, execution);
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

/// One session against one region's Athena.
///
/// There is no connection to hold: every call is a signed POST and `Wire` is a
/// pooled hyper client. That is what makes `cancel` an ordinary second request,
/// and it is also why `transactional` is false — see `driver.rs`.
pub struct AthenaSource {
    wire: Arc<Wire>,
    /// The data catalog this connection reads, which is the level above the
    /// database. Fixed by the connection string rather than navigable, because
    /// a federated catalog is a different Lambda function with different
    /// permissions and listing them all is a separate right — so `schemas()`
    /// answers for one catalog and the string says which.
    catalog: String,
    /// The database unqualified names resolve in, or empty where the string
    /// named none.
    database: String,
    /// **Where the query runs, is metered and is billed.** A workgroup is
    /// Athena's unit of isolation: it carries the engine version, the data-scan
    /// limit, the encryption settings and the CloudWatch metrics, and an account
    /// that separates teams separates them here. So it belongs in a connection
    /// string for the same reason a Trino catalog does — two connections to the
    /// same region that differ only in workgroup are two genuinely different
    /// things to be connected to.
    workgroup: String,
    /// **Where the answer is written**, which is a separate question from where
    /// the query runs, and the reason both are in the string.
    ///
    /// Athena has no result store of its own: every statement writes a file to
    /// S3 and `GetQueryResults` reads that file back. A query with nowhere to
    /// write fails, so somebody has to say where — either the workgroup does, by
    /// carrying an output location of its own, or the connection does. `connect`
    /// establishes which before the first statement rather than after it, and
    /// this is `None` exactly when the workgroup is the one answering.
    output: Option<String>,
    live: Live,
    next: AtomicU64,
    /// Shared by every result this session hands out through `query`. A cursor
    /// gets one of its own — the trait says a session cancel does not reach a
    /// cursor.
    stop: Arc<Stop>,
}

impl AthenaSource {
    /// Connects to `url`, of the form
    /// `athena://[<key id>:<secret>@]<region>/<database>?workgroup=<name>&output=s3://…`.
    ///
    /// The region is in the host position, which is the one uncomfortable thing
    /// about this string and is stated rather than hidden: an Athena connection
    /// has no host to name — the endpoint is derived from the region — and the
    /// region is what is actually being chosen. The key id and secret are in the
    /// user and password fields, which for once fits exactly: they are a name
    /// and a secret, and the form already has a box for each.
    ///
    /// `catalog` may also be given and defaults to `AwsDataCatalog`.
    ///
    /// The round trip at the end is `GetWorkGroup`, and it proves four things
    /// that would otherwise each fail later and less clearly: that the
    /// credentials are real, that the region is one that has Athena, that the
    /// workgroup exists and is enabled, and — the one worth the request —
    /// whether anybody has said where results go. A driver that only proved
    /// reachability would report success and then fail on the first statement
    /// with *No output location provided*, which is a true message about a
    /// connection dialog that was filled in ten minutes earlier.
    pub async fn connect(url: &str) -> Result<Self, AthenaError> {
        let parsed =
            url::Url::parse(url).map_err(|e| AthenaError::BadUrl(format!("{url}: {e}")))?;
        let typed = Keys {
            access_key_id: percent_decode(parsed.username()),
            secret_access_key: percent_decode(parsed.password().unwrap_or_default()),
            session_token: None,
            region: percent_decode(parsed.host_str().unwrap_or_default()),
        };
        let database = percent_decode(parsed.path().trim_matches('/'));
        let query: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();

        let keys = credentials::resolve(typed, &|name| std::env::var(name).ok(), |path| {
            std::fs::read_to_string(path).ok()
        })?;
        if keys.region.is_empty() {
            return Err(AthenaError::BadUrl(
                "this connection names no region and nothing else does either: write \
                 athena://<region>/<database>, or export AWS_REGION"
                    .to_string(),
            ));
        }

        let workgroup = query
            .get("workgroup")
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_WORKGROUP.to_string());
        let catalog = query
            .get("catalog")
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_CATALOG.to_string());
        let asked_output = query
            .get("output")
            .filter(|location| !location.is_empty())
            .cloned();

        let wire = Arc::new(Wire::new(Signer {
            access_key_id: keys.access_key_id,
            secret_access_key: keys.secret_access_key,
            session_token: keys.session_token,
            region: keys.region,
            service: "athena".to_string(),
        }));

        let group: wire::WorkGroupDetail = wire
            .call(
                "GetWorkGroup",
                serde_json::json!({ "WorkGroup": workgroup }),
            )
            .await?;
        let group = group.work_group;
        if group.state == "DISABLED" {
            return Err(AthenaError::Query {
                message: format!(
                    "the workgroup {workgroup} is disabled, so no statement sent to it will run"
                ),
                kind: "InvalidRequestException".to_string(),
                position: None,
            });
        }

        // Which of the two says where results go, decided once. A workgroup that
        // enforces its configuration overrides whatever a client sends, so
        // sending an output location to one is at best ignored and at worst
        // confusing to whoever reads the request later; and a workgroup that
        // carries a location without enforcing it has already answered the
        // question. So the connection's own location is sent only when it is
        // the only answer there is.
        let enforced = group.configuration.enforce_work_group_configuration;
        let group_output = group
            .configuration
            .result_configuration
            .output_location
            .filter(|location| !location.is_empty());
        let output = match (&asked_output, &group_output, enforced) {
            (_, Some(_), true) => None,
            (Some(location), _, _) => Some(location.clone()),
            (None, Some(_), false) => None,
            (None, None, _) => {
                return Err(AthenaError::BadUrl(format!(
                    "nothing says where results go: the workgroup {workgroup} has no output \
                     location, so this connection needs one — \
                     athena://…?output=s3://bucket/prefix/"
                )));
            }
        };

        Ok(Self {
            wire,
            catalog,
            database,
            workgroup,
            output,
            live: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
            stop: Arc::new(Stop::default()),
        })
    }

    /// The data catalog this connection reads.
    pub fn catalog(&self) -> &str {
        &self.catalog
    }

    /// The database unqualified names resolve in, or empty where there is none.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// The region this connection's endpoint is in.
    pub fn region(&self) -> &str {
        self.wire.region()
    }

    pub(crate) fn wire(&self) -> &Wire {
        &self.wire
    }

    /// Runs `sql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the columns are known and before any row is handed over.
    /// That costs waiting out the whole execution, which is the honest price of
    /// Athena's shape: there is no `DESCRIBE` that is not itself a query, and
    /// the columns arrive with the first page of results, which does not exist
    /// until the statement has finished.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<Rows, AthenaError> {
        Rows::open(
            self,
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
    /// The same mechanism as `query`, because a finished query execution already
    /// is a cursor: the rows are a file, `NextToken` is a position in it, and
    /// reading forward re-reads nothing and cannot see a later write. What
    /// differs is that this one carries a `Stop` of its own and is not
    /// registered with the session, so `cancel` does not reach it.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Rows, AthenaError> {
        Rows::open(self, sql, batch_rows, Arc::new(Stop::default()), None).await
    }

    /// Asks Athena to abandon whatever this session is running.
    ///
    /// One `StopQueryExecution` per statement in flight, because there is no
    /// connection and a stop contends with nothing. The local stop goes first so
    /// that a reader parked on a page is released whatever happens to the
    /// requests that follow.
    ///
    /// Best-effort, as the trait says. A session with nothing running sends
    /// nothing at all.
    pub async fn cancel(&self) -> Result<(), AthenaError> {
        self.stop.stop();
        let executions: Vec<String> = match self.live.lock() {
            Ok(live) => live.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        for execution in executions {
            stop_execution(&self.wire, &execution).await?;
        }
        Ok(())
    }
}

/// Asks Athena to stop one execution.
///
/// Naming one that has already finished is not an error — the service answers
/// successfully and does nothing — which is what makes stopping an idle session
/// harmless.
async fn stop_execution(wire: &Wire, execution: &str) -> Result<(), AthenaError> {
    let _: serde_json::Value = wire
        .call(
            "StopQueryExecution",
            serde_json::json!({ "QueryExecutionId": execution }),
        )
        .await?;
    Ok(())
}

fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// A result being read forward, in pages of the size that was asked for.
///
/// Both a `ResultStream` and a `Cursor`; see `AthenaSource::cursor`.
pub struct Rows {
    wire: Arc<Wire>,
    /// The text the caller wrote, kept so a failure's offset resolves against
    /// what they can see.
    text: String,
    plan: Plan,
    execution: String,
    /// The next page's token, or `None` once Athena has stopped offering one —
    /// which is what "finished" means here.
    next: Option<String>,
    /// Rows read out of a page but not yet handed over.
    ///
    /// `GetQueryResults` returns at most 1000 rows, so the carry has to work in
    /// both directions — accumulate pages that are too small, split pages that
    /// are too large — exactly as the Trino driver's does.
    carry: Option<RecordBatch>,
    batch_rows: usize,
    delivered: u64,
    /// Rows an `INSERT INTO` wrote, which Athena counts for itself.
    update_count: Option<u64>,
    stop: Arc<Stop>,
    /// The generation this result began in; see `Stop`.
    since: u64,
    _registration: Option<Registration>,
}

impl Rows {
    async fn open(
        source: &AthenaSource,
        sql: &str,
        batch_rows: usize,
        stop: Arc<Stop>,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, AthenaError> {
        let since = stop.now();
        let mut request = serde_json::json!({
            "QueryString": sql,
            "WorkGroup": source.workgroup,
            "QueryExecutionContext": { "Catalog": source.catalog },
        });
        if !source.database.is_empty() {
            request["QueryExecutionContext"]["Database"] = serde_json::json!(source.database);
        }
        if let Some(location) = &source.output {
            request["ResultConfiguration"] = serde_json::json!({ "OutputLocation": location });
        }

        let started: Started = source
            .wire
            .call("StartQueryExecution", request)
            .await
            .map_err(|e| e.about(sql))?;
        // Held from here rather than from the first page, so that a statement
        // cancelled while it is still `QUEUED` is one `cancel` can name.
        let registration = register
            .map(|(live, id)| Registration::hold(live, id, started.query_execution_id.clone()));

        let statement_type = wait_for(&source.wire, &started.query_execution_id, &stop, since)
            .await
            .map_err(|e| e.about(sql))?;

        let mut rows = Rows {
            wire: Arc::clone(&source.wire),
            text: sql.to_string(),
            plan: Plan::empty(),
            execution: started.query_execution_id,
            next: None,
            carry: None,
            batch_rows: batch_rows.max(1),
            delivered: 0,
            update_count: None,
            stop,
            since,
            _registration: registration,
        };

        // The first page settles the columns, so it is read here rather than
        // left for `next_page`: `query` promises the columns before any row is
        // handed over. One row more than asked for, because a `DML`'s first page
        // spends a row on the repeated header and a caller who asked for a
        // hundred rows should get a hundred.
        let page: Results = rows
            .page(None, rows.batch_rows.saturating_add(1))
            .await
            .map_err(|e| e.about(sql))?;
        rows.plan = Plan::of(&page.result_set.result_set_metadata.column_info);
        rows.next = (!page.next_token.is_empty()).then_some(page.next_token);
        rows.update_count = page.update_count;

        let mut data = page.result_set.rows;
        // The header, dropped on both conditions; `arrow_map::Plan::is_header`
        // argues why it is both and not either.
        if statement_type == "DML" && data.first().is_some_and(|first| rows.plan.is_header(first)) {
            data.remove(0);
        }
        rows.absorb(&data)?;
        Ok(rows)
    }

    pub fn schema(&self) -> SchemaRef {
        self.plan.schema()
    }

    /// Rows this statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Two answers, because Athena gives two. A write reports what it changed,
    /// which arrives as `UpdateCount` — the same number Trino's `updateCount`
    /// carries and a better one than the Flight SQL driver can offer. A read
    /// reports what it produced, counted here.
    pub fn rows_affected(&self) -> Option<u64> {
        if self.next.is_some() || self.carry.is_some() {
            return None;
        }
        Some(self.update_count.unwrap_or(self.delivered))
    }

    /// The next page, or `None` once the result is fully consumed.
    ///
    /// A read that has been stopped stays stopped, which is why the check is
    /// here as well as around the request: a page already buffered would
    /// otherwise still be handed over after Cancel.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, AthenaError> {
        if self.stop.now() != self.since {
            return Err(AthenaError::Cancelled(STOPPED));
        }
        loop {
            let held = self.carry.as_ref().map_or(0, RecordBatch::num_rows);
            if held >= self.batch_rows {
                return Ok(Some(self.take(self.batch_rows)));
            }
            let Some(token) = self.next.clone() else {
                return Ok((held > 0).then(|| self.take(held)));
            };

            let want = self.batch_rows.saturating_sub(held);
            let page: Results = self
                .page(Some(token), want)
                .await
                .map_err(|e| e.about(&self.text))?;
            self.next = (!page.next_token.is_empty()).then_some(page.next_token);
            if page.update_count.is_some() {
                self.update_count = page.update_count;
            }
            // Only the first page repeats the header, so nothing is dropped
            // here — and a page after the first that happened to hold the column
            // names would be data.
            self.absorb(&page.result_set.rows)?;
        }
    }

    /// One `GetQueryResults`, bounded by the service's ceiling.
    async fn page(&self, token: Option<String>, want: usize) -> Result<Results, AthenaError> {
        let mut request = serde_json::json!({
            "QueryExecutionId": self.execution,
            "MaxResults": want.clamp(1, PAGE_CEILING),
        });
        if let Some(token) = token {
            request["NextToken"] = serde_json::json!(token);
        }
        let stop = Arc::clone(&self.stop);
        let since = self.since;
        tokio::select! {
            biased;
            () = stop.stopped(since) => Err(AthenaError::Cancelled(STOPPED)),
            answer = self.wire.call("GetQueryResults", request) => answer,
        }
    }

    /// Adds one page's rows to the carry.
    fn absorb(&mut self, rows: &[Row]) -> Result<(), AthenaError> {
        if rows.is_empty() || self.plan.columns() == 0 {
            return Ok(());
        }
        let batch = self.plan.batch(rows)?;
        self.carry = match self.carry.take() {
            None => Some(batch),
            Some(held) => Some(arrow::compute::concat_batches(
                &self.plan.schema(),
                &[held, batch],
            )?),
        };
        Ok(())
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

    /// A handle for stopping this reader from another thread.
    ///
    /// Taken out in advance rather than reached for at cancel time, because by
    /// then the reader is borrowed by the fetch that is to be stopped. The
    /// execution id it names was chosen by Athena before the first page existed
    /// and does not move, which is why it and not the page token is what a stop
    /// is addressed to.
    pub fn canceller(&self) -> RowsCancel {
        RowsCancel {
            wire: Arc::clone(&self.wire),
            stop: Arc::clone(&self.stop),
            execution: self.execution.clone(),
        }
    }

    /// Lets go of the paging and of whatever is held.
    ///
    /// Optional; dropping does the same. Note what neither does: Athena is not
    /// told. The execution has finished by the time rows are being read, and its
    /// results are a file in S3 that goes on existing whether anybody reads it
    /// or not — which is a difference from every other driver here, and the one
    /// place an abandoned result costs storage rather than a server-side
    /// resource.
    pub async fn close(&mut self) -> Result<(), AthenaError> {
        self.next = None;
        self.carry = None;
        self._registration = None;
        Ok(())
    }
}

/// Stops the statement one reader is running.
#[derive(Clone)]
pub struct RowsCancel {
    wire: Arc<Wire>,
    stop: Arc<Stop>,
    execution: String,
}

impl RowsCancel {
    /// Delivered is not interrupted, as everywhere else: a statement that had
    /// already finished leaves nothing to stop and this still succeeds.
    ///
    /// The local stop goes first because it cannot fail and costs nothing, so a
    /// reader parked on a page is released whatever happens to the request that
    /// follows.
    pub async fn cancel(&self) -> Result<(), AthenaError> {
        self.stop.stop();
        stop_execution(&self.wire, &self.execution).await
    }
}

/// Waits for an execution to leave the running states, and answers with how
/// Athena classified the statement.
///
/// The classification is carried out of here rather than fetched again because
/// it is only in this answer, and `Rows::open` needs it for the header rule.
///
/// The wait happens here rather than inside one long request for the reason the
/// Trino driver's `wire.rs` gives about retries: a request this side is parked
/// on is one the Cancel button cannot reach.
async fn wait_for(
    wire: &Wire,
    execution: &str,
    stop: &Stop,
    since: u64,
) -> Result<String, AthenaError> {
    let mut interval = POLL_FIRST;
    loop {
        let state: Execution = wire
            .call(
                "GetQueryExecution",
                serde_json::json!({ "QueryExecutionId": execution }),
            )
            .await?;
        let detail = state.query_execution;
        match detail.status.state.as_str() {
            "SUCCEEDED" => return Ok(detail.statement_type),
            // Somebody stopped it — here, or from the console, or through
            // another client. Either way it is not a fault of the statement's.
            "CANCELLED" => return Err(AthenaError::Cancelled(STOPPED)),
            "FAILED" => {
                let error = detail.status.athena_error.unwrap_or_default();
                let message = [error.error_message, detail.status.state_change_reason]
                    .into_iter()
                    .find(|text| !text.is_empty())
                    .unwrap_or_else(|| {
                        "this statement failed and Athena said nothing about why".to_string()
                    });
                return Err(AthenaError::Query {
                    message,
                    // Athena's categories are 1 for a system fault, 2 for the
                    // user's and 3 for something else's. The word rather than
                    // the number, because this reaches a person.
                    kind: match error.error_category {
                        1 => "system".to_string(),
                        2 => "user".to_string(),
                        3 => "other".to_string(),
                        _ => "failed".to_string(),
                    },
                    position: None,
                });
            }
            _ => {}
        }
        tokio::select! {
            biased;
            () = stop.stopped(since) => return Err(AthenaError::Cancelled(STOPPED)),
            () = tokio::time::sleep(interval) => {}
        }
        interval = (interval * 2).min(POLL_LONGEST);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL that is not one is refused before anything is signed or read from
    /// disk, which is what makes this test need nothing on the machine.
    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_before_anything_is_sent() {
        let error = AthenaSource::connect("not a url at all")
            .await
            .err()
            .expect("that is not a URL");
        assert!(matches!(error, AthenaError::BadUrl(_)), "{error}");
    }

    /// The caret, out of the only place Athena's engine puts one.
    ///
    /// The assertion is that the offset lands on the `O` of `ORDER`, which is
    /// true of exactly one reading of `line 1:35`.
    #[test]
    fn a_position_is_read_out_of_the_engines_prose() {
        let sql = "SELECT id FROM sales.orders WHERE ORDER BY id";
        let message = "SYNTAX_ERROR: line 1:35: mismatched input 'ORDER'. Expecting: '(', …";
        let at = statement_position(message, sql).expect("an offset") as usize;
        assert_eq!(sql.chars().nth(at - 1), Some('O'));
    }

    /// The column is counted in characters, which is what the Trino driver
    /// measured against the same engine — and pinning it here is what turns a
    /// day with an account into a failing test rather than a caret quietly
    /// landing in the wrong place.
    #[test]
    fn a_position_is_counted_in_characters_and_not_bytes() {
        // Six CJK characters ahead of the fault. Three bytes each, so a byte
        // offset for the same character would have been 42 rather than 30.
        let sql = "SELECT \"漢字漢字漢字\" FROM t WHERE ORDER BY id";
        let at =
            statement_position("line 1:30: mismatched input", sql).expect("an offset") as usize;
        assert_eq!(sql.chars().nth(at - 1), Some('O'));
    }

    /// A later line counts the lines before it, which is two chances to be off
    /// by one.
    #[test]
    fn an_offset_on_a_later_line_counts_the_lines_before_it() {
        let sql = "SELECT id FROM t\nWHERE ORDER BY id";
        let at = statement_position("line 2:7: mismatched input", sql).expect("an offset") as usize;
        assert_eq!(sql.chars().nth(at - 1), Some('O'));
    }

    /// The first offset and not the last, which is the opposite of what the
    /// BigQuery driver does with its own messages: Athena puts the position in
    /// front and can quote the statement afterwards.
    #[test]
    fn the_first_offset_is_the_one_that_is_read() {
        let sql = "SELECT 1";
        let message = "SYNTAX_ERROR: line 1:8: mismatched input near line 1:1";
        assert_eq!(statement_position(message, sql), Some(8));
    }

    /// A message with no offset, and one whose offset cannot be this statement.
    #[test]
    fn a_message_with_no_offset_produces_none() {
        assert_eq!(
            statement_position("TABLE_NOT_FOUND: line item does not exist", "SELECT 1"),
            None,
            "the word 'line' in ordinary prose is not an offset"
        );
        assert_eq!(statement_position("line 9:1:", "SELECT 1"), None);
        assert_eq!(statement_position("line 1:0:", "SELECT 1"), None);
        // One past the end is where an end-of-input fault points; further than
        // that is clamped to it rather than pointing outside the statement.
        assert_eq!(statement_position("line 1:9:", "SELECT 1"), Some(9));
        assert_eq!(statement_position("line 1:40:", "SELECT 1"), Some(9));
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
