//! Databricks, read over the SQL Statement Execution API as Arrow.
//!
//! **No Databricks workspace has ever answered this driver.** There is no
//! workspace behind it, no trial, no container and no emulator; every line of it
//! is read off the published API reference and written without a single request
//! having been sent. Every other driver in this workspace earned its place by
//! being run against the real thing — that is what the contract suite in
//! `crates/conn/tests/contract.rs` is, and it is why this driver has no subject
//! there. An absent subject is the honest report; a mocked one would make a
//! suite green and establish nothing.
//!
//! ## What will only be known when a server answers
//!
//! 1. **Whether a service principal gets a token, and whether `all-apis` is the
//!    scope to ask for.** `auth.rs` tests the two halves that are arithmetic —
//!    the Basic header, and reading an expiry — and cannot test the exchange.
//! 2. **Whether each result chunk is a complete Arrow stream.** This driver
//!    decodes every presigned link with a decoder of its own, which is what makes
//!    chunks independently fetchable and is why the API can hand out several at
//!    once. If a chunk after the first carries batches without a schema message,
//!    it fails with `Missing schema` rather than producing anything.
//! 3. **Whether the Arrow bodies are compressed.** Arrow IPC declares LZ4 or
//!    ZSTD in the message header, and this build has neither decoder compiled in,
//!    so a compressed body fails loudly and names the codec. Turning one on is a
//!    feature flag in `Cargo.toml`; guessing that it is needed would be adding a
//!    dependency for a case nobody has seen.
//! 4. **Whether `system.information_schema.schemata` can be read.** It is the
//!    one view that answers for every catalog in the metastore, and it is what
//!    the navigator's root is built from — so a metastore whose system catalog is
//!    not enabled shows an empty tree with the workspace's own message behind it.
//! 5. **Whether Unity Catalog's `information_schema` really has
//!    `full_data_type`, `position_in_unique_constraint` and `check_clause`.**
//!    `metadata.rs` reads all three by name, and a missing one is a statement
//!    that fails rather than a value that is wrong.
//! 6. **The polling schedule.** How long a warehouse takes to move a statement
//!    from `PENDING` to `SUCCEEDED` was guessed, not measured.
//! 7. **Whether a presigned link refuses the workspace's bearer token.** This
//!    driver omits it, which is what S3 and Azure Blob require of a request that
//!    is already signed in its query string; see `Wire::fetch`.
//!
//! One thing is *not* on that list, and it is worth saying which: the
//! `ordinal_position` a catalog counts columns from. It would have been a guess,
//! so `metadata.rs` numbers columns by the order the rows arrive in instead, and
//! the question stops mattering.
//!
//! ## Why this driver is in this phase
//!
//! Because the result can already be Arrow. `disposition: EXTERNAL_LINKS` with
//! `format: ARROW_STREAM` has the warehouse write Arrow IPC to cloud storage and
//! answer with presigned URLs; the bytes never pass through the control plane and
//! never stop being Arrow. There is no `arrow_map.rs` here for the same reason
//! there is none in the Flight SQL driver — the schema and the values come off
//! the wire already described, and a type mapping would be this driver's second
//! opinion about columns Arrow has stated.
//!
//! The decode is the Flight SQL driver's, in the one respect that matters: the
//! body arrives as an owned `bytes::Bytes` and goes to `Buffer::from` whole, so
//! the arrays built out of it are windows into what the socket read rather than
//! copies of it. What that is worth in practice is exactly what the Flight SQL
//! driver measured and this one cannot — 95% of a real result, there, with the
//! remainder being fixed-width buffers that alignment forced. The mechanism is
//! the same; the number is not claimed.
//!
//! **A catalog query does not go through cloud storage.** `ask` runs with
//! `INLINE` and `JSON_ARRAY` instead, because a dozen rows of table names are not
//! worth a presigned round trip — the same split the Trino driver makes between
//! its Arrow path and its metadata path, arrived at here for a different reason.
//!
//! ## What the protocol is
//!
//! Three levels of namespace — `catalog.schema.table` — where the trait has two,
//! so a schema here is called `catalog.schema`, as in the DuckDB, Trino and
//! Snowflake drivers.
//!
//! A cursor and a query are the same call, for the fourth distinct reason in this
//! workspace: a finished statement's result is a chain of chunks, each naming the
//! next, read forward once. Page *n* costs what page one costs and nothing is
//! re-read, which is both properties the trait asks a cursor for.
//!
//! There is no session. Every statement carries its own warehouse, catalog and
//! schema, so a `USE CATALOG` typed into the editor succeeds and changes nothing
//! after it — and `transactional` is false, which for this database is not a
//! limitation of the transport: `driver.rs` says why.

mod auth;
mod driver;
mod metadata;
mod wire;

use arrow::array::RecordBatch;
use arrow::buffer::Buffer;
use arrow::datatypes::{Schema, SchemaRef};
use arrow::ipc::reader::StreamDecoder;
use auth::{Credential, Machine};
// Only named by the test decode below: everywhere else the bytes go from
// `Wire::fetch` straight into `Buffer::from` without this side spelling the type.
#[cfg(test)]
use bytes::Bytes;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wire::{Delivery, Session, Statement, Wire};

/// How long to wait before asking a running statement whether it is done, and
/// the ceiling that wait doubles up to.
///
/// Guessed rather than measured, and the guess is stated here rather than buried
/// in the loop. A warehouse that is awake answers a small statement in well under
/// a second; one that is starting up takes minutes, and should not be asked about
/// hundreds of times while it does.
const FIRST_POLL: Duration = Duration::from_millis(100);
const LONGEST_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum DatabricksError {
    /// A statement the warehouse ran and did not finish — a compilation error, a
    /// permission, or somebody pressing Cancel.
    #[error("{message}")]
    Statement { message: String, cancelled: bool },
    /// A request that did not get an answer, or got one that was not the API's:
    /// no route, a TLS failure, an expired presigned link.
    #[error("{0}")]
    Transport(String),
    /// Something wrong with the credentials — a connection string naming none, a
    /// token endpoint that refused.
    #[error("{0}")]
    Auth(String),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl DatabricksError {
    /// Whether the warehouse stopped this statement because somebody asked it
    /// to.
    ///
    /// Read from the state the warehouse reported and not from this side
    /// remembering that it pressed Cancel: a statement can fail on its own merits
    /// in the same moment the cancel lands, and `CANCELED` is a state of its own
    /// beside `FAILED` — which makes this the least ambiguous cancel of the three
    /// REST drivers here.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, DatabricksError::Statement { cancelled, .. } if *cancelled)
    }
}

/// A `catalog.schema` name split back into the two levels Unity Catalog has.
///
/// At the first dot, for the reason the Trino driver gives: the catalog is the
/// half that is named first and the schema is the half that is reached last, so
/// a dot that is in neither's name belongs to the second.
///
/// `None` where there is no dot, which is a schema string that never came from
/// `schemas()`. Every caller answers that with an empty result rather than a
/// statement naming a catalog that is not there.
pub(crate) fn parts(schema: &str) -> Option<(&str, &str)> {
    schema.split_once('.')
}

/// A name as Databricks spells one, for the statements this driver writes
/// itself.
///
/// Backticks, always quoted, and unlike Snowflake this is a convenience rather
/// than a correctness matter: Unity Catalog compares names case-insensitively, so
/// a bare name finds the right relation whatever its case. It is unconditional
/// anyway, because a catalog name that is a reserved word — and `system` nearly
/// is — would otherwise be a syntax error in a statement nobody reads.
pub(crate) fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// A value as a SQL string literal.
///
/// Interpolated rather than bound, as in the Trino and Snowflake drivers and for
/// the same trade: the API does carry named parameters, and buying escaping that
/// way means every catalog query below becoming a second structure to build — to
/// replace one function whose whole content is doubling a quote. Databricks also
/// has backslash escapes inside string literals by default, so the backslash is
/// doubled too; that is the one place this differs from the other two.
pub(crate) fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

/// The statements this session has in flight, by id.
///
/// A `std::sync::Mutex` and not tokio's, because a reader removes its own entry
/// from `Drop`, which cannot await.
type Live = Arc<Mutex<HashMap<u64, String>>>;

/// One session against one Databricks SQL warehouse.
///
/// There is no connection to hold: the API is stateless HTTPS. That is what makes
/// `cancel` an ordinary second request rather than something needing a connection
/// of its own, and it is why a cursor can be paged while another statement runs.
pub struct DatabricksSource {
    wire: Arc<Wire>,
    live: Live,
    next: AtomicU64,
}

impl DatabricksSource {
    /// Connects to `url`, of the form
    /// `https://host/catalog/schema?warehouse_id=abc123&token=dapi…`.
    ///
    /// The warehouse is not optional — the API refuses a statement that names
    /// none and has no default — so a connection string without one is refused
    /// here rather than at the first click. The catalog and schema are optional
    /// and become the defaults sent with every statement.
    ///
    /// One of two credentials has to be there. `token=` is a personal access
    /// token. `client_id=` with `client_secret=` is a service principal, which is
    /// the machine-to-machine half of this phase's exit condition: no browser, no
    /// redirect, no device code — one form post for a bearer token that lasts an
    /// hour.
    ///
    /// The round trip at the end proves the thing a connection dialog is actually
    /// asking: not that the host resolves, but that the workspace read the
    /// credentials, that the warehouse id names a warehouse, and that it can run
    /// a statement. `SELECT 1` is the smallest statement that establishes all
    /// three, and it costs a warehouse start if the warehouse was asleep — which
    /// is a cost worth paying at the dialog rather than discovering later.
    pub async fn connect(url: &str) -> Result<Self, DatabricksError> {
        let parsed =
            url::Url::parse(url).map_err(|e| DatabricksError::BadUrl(format!("{url}: {e}")))?;
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| DatabricksError::BadUrl(format!("{url}: no host")))?;
        let origin = match parsed.port() {
            Some(port) => format!("https://{host}:{port}"),
            None => format!("https://{host}"),
        };

        let mut path = parsed
            .path()
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(percent_decode);
        let mut session = Session {
            catalog: path.next().unwrap_or_default(),
            schema: path.next().unwrap_or_default(),
            ..Session::default()
        };

        let mut token = None;
        let mut client_id = String::new();
        let mut secret = String::new();
        for (name, value) in parsed.query_pairs() {
            match name.as_ref() {
                "warehouse_id" => session.warehouse = value.into_owned(),
                "token" => token = Some(value.into_owned()),
                "client_id" => client_id = value.into_owned(),
                "client_secret" => secret = value.into_owned(),
                // Anything else is left alone rather than refused: the API takes
                // parameters this driver has no opinion about, and a connection
                // string that names one should not be rejected by the client
                // that would have passed it on.
                _ => {}
            }
        }

        if session.warehouse.is_empty() {
            return Err(DatabricksError::BadUrl(
                "this connection names no SQL warehouse. Add warehouse_id=…, which is the last \
                 part of the warehouse's HTTP path in the workspace"
                    .to_string(),
            ));
        }
        let credential = match (token, client_id.is_empty()) {
            (Some(token), _) => Credential::Token(token),
            (None, false) => Credential::Machine(Machine::new(&client_id, &secret)),
            (None, true) => {
                return Err(DatabricksError::Auth(
                    "this connection names no credentials. Add token=… for a personal access \
                     token, or client_id=… with client_secret=… for a service principal"
                        .to_string(),
                ));
            }
        };

        let source = Self {
            wire: Arc::new(Wire::new(origin, credential, session)),
            live: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
        };
        source.ask("SELECT 1").await?;
        Ok(source)
    }

    /// The catalog unqualified names resolve in, or empty where there is none.
    pub fn catalog(&self) -> &str {
        &self.wire.session().catalog
    }

    /// The schema unqualified names resolve in, or empty where there is none.
    pub fn schema_name(&self) -> &str {
        &self.wire.session().schema
    }

    /// Runs `sql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the statement has finished and the first chunk of Arrow has
    /// been read, which is what makes the columns known before any row is handed
    /// over. That is later than the Trino driver's promise and is the API's doing
    /// rather than a choice: there is no answer describing the columns before the
    /// result exists, and the only description this driver will accept is the
    /// Arrow schema itself.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<Rows, DatabricksError> {
        self.read(
            sql,
            batch_rows,
            Some((
                Arc::clone(&self.live),
                self.next.fetch_add(1, Ordering::Relaxed),
            )),
        )
        .await
    }

    /// Reads `sql` forward, a chunk at a time.
    ///
    /// The same mechanism as `query`, for the reason in the crate comment: the
    /// chain of chunk links already is one execution read forward without
    /// re-reading. What differs is that this one is not registered with the
    /// session, so `cancel` does not reach it — the trait says a session cancel
    /// does not touch a cursor, and this is where that is true rather than
    /// remembered.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Rows, DatabricksError> {
        self.read(sql, batch_rows, None).await
    }

    async fn read(
        &self,
        sql: &str,
        batch_rows: usize,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, DatabricksError> {
        Rows::open(Arc::clone(&self.wire), sql, batch_rows.max(1), register).await
    }

    /// Asks the warehouse to abandon whatever this session is running.
    ///
    /// One cancel per statement in flight, because HTTPS is stateless and a
    /// cancel contends with nothing. Best-effort, as the trait says: success
    /// means the request was delivered, not that anything stopped.
    pub async fn cancel(&self) -> Result<(), DatabricksError> {
        let ids: Vec<String> = match self.live.lock() {
            Ok(live) => live.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        for id in ids {
            self.wire.cancel(&id).await?;
        }
        Ok(())
    }

    /// Runs one catalog statement and hands back its rows as they arrived.
    ///
    /// JSON and not Arrow, and delivered inline rather than through cloud
    /// storage: `metadata.rs` wants strings out of a handful of columns, and both
    /// a type mapping and a presigned round trip would be in the path of every
    /// navigator click.
    pub(crate) async fn ask(&self, sql: &str) -> Result<Catalog, DatabricksError> {
        let started = self.wire.post(sql, Delivery::Inline).await?;
        let finished = settle(&self.wire, started).await?;
        let manifest = finished.manifest.as_ref();
        check_format(manifest.and_then(|m| m.format.as_deref()), Delivery::Inline)?;

        let names: Vec<String> = manifest
            .and_then(|m| m.schema.as_ref())
            .map(|schema| schema.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();

        let mut rows = Vec::new();
        let mut chunk = finished.result.unwrap_or_default();
        loop {
            if let Some(data) = chunk.data_array.take() {
                rows.extend(data);
            }
            let Some(next) = chunk.next().map(str::to_string) else {
                break;
            };
            chunk = self.wire.chunk(&next).await?;
        }
        Ok(Catalog { names, rows })
    }
}

/// One catalog answer: the rows, and what the columns were called.
///
/// Read by name rather than by position, so that a statement below can name the
/// columns it wants and be read the same way whatever order they arrive in.
pub(crate) struct Catalog {
    names: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Catalog {
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// Which column `name` is, or `None` where the answer has none.
    pub fn at(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|column| column == name)
    }

    /// One column of one row as text, empty where it is null or absent.
    ///
    /// Empty rather than an error, as in the Trino driver: an empty name is
    /// visible in the navigator, where a failed refresh is not. A `JSON_ARRAY`
    /// result states every value as a string, so the other arms are for a
    /// warehouse that does something else with a number.
    pub fn text(&self, row: &[Value], at: Option<usize>) -> String {
        match at.and_then(|at| row.get(at)) {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    }
}

fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// Waits for a statement to stop running, and refuses it if it stopped badly.
///
/// The API does not hold the request open — a `GET` on a running statement
/// answers straight away with its state — so this is a poll and there is no
/// version of it that is not. The schedule doubles from a tenth of a second to
/// two; it is a guess, and the crate comment says so.
async fn settle(wire: &Wire, started: Statement) -> Result<Statement, DatabricksError> {
    let id = started.statement_id.clone().ok_or_else(|| {
        DatabricksError::Transport(
            "the workspace started a statement without naming it, so nothing here could read \
             it or stop it"
                .to_string(),
        )
    })?;

    let mut statement = started;
    let mut wait = FIRST_POLL;
    loop {
        let status = statement.status.as_ref().ok_or_else(|| {
            DatabricksError::Transport("the workspace answered without a state".to_string())
        })?;
        let message = || {
            status
                .error
                .as_ref()
                .and_then(|failure| failure.message.clone())
        };
        match status.state.as_str() {
            "SUCCEEDED" => return Ok(statement),
            "PENDING" | "RUNNING" => {}
            // A state of its own beside `FAILED`, which is what makes this the
            // least ambiguous cancel of the three REST drivers here: no error
            // code to key on and no guessing from a message.
            "CANCELED" => {
                return Err(DatabricksError::Statement {
                    message: message()
                        .unwrap_or_else(|| "this statement was cancelled".to_string()),
                    cancelled: true,
                });
            }
            other => {
                return Err(DatabricksError::Statement {
                    // `FAILED` carries the warehouse's own message and `CLOSED`
                    // does not — a closed statement is one whose result was
                    // already fetched or expired, which is a sentence this side
                    // has to supply.
                    message: message().unwrap_or_else(|| {
                        format!("this statement ended in state {other} and said nothing")
                    }),
                    cancelled: false,
                });
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(LONGEST_POLL);
        statement = wire.poll(&id).await?;
    }
}

/// Refuses a result delivered in a form this driver cannot read.
///
/// Loud rather than discovered further down. A `JSON_ARRAY` answer handed to the
/// Arrow decoder fails with something about an IPC continuation marker, which
/// says nothing about what actually happened.
fn check_format(format: Option<&str>, delivery: Delivery) -> Result<(), DatabricksError> {
    match format {
        None => Ok(()),
        Some(format) if format == delivery.format() => Ok(()),
        Some(format) => Err(DatabricksError::Transport(format!(
            "this result is encoded as {format}, and {} was asked for",
            delivery.format()
        ))),
    }
}

/// Puts a statement's id where `DatabricksSource::cancel` can find it, and takes
/// it back out when the reader is dropped.
struct Registration {
    id: u64,
    live: Live,
}

impl Registration {
    fn hold(live: Live, id: u64, statement: String) -> Self {
        if let Ok(mut held) = live.lock() {
            held.insert(id, statement);
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

/// A result being read forward, in pages of the size that was asked for.
///
/// Both a `ResultStream` and a `Cursor`, because against this API they are the
/// same object read the same way.
pub struct Rows {
    wire: Arc<Wire>,
    statement: String,
    schema: SchemaRef,
    /// Presigned links known but not yet fetched, oldest first.
    links: VecDeque<String>,
    /// The API path of the chunk after the ones in `links`, or `None` once every
    /// chunk has been named.
    next_chunk: Option<String>,
    /// Batches decoded but not yet handed over, oldest first.
    ///
    /// A queue rather than one buffer, for the reason the Flight SQL driver
    /// gives: the warehouse's batch size is not the caller's page size, and
    /// holding the arrivals apart keeps the one page that straddles a boundary
    /// from being a copy that spreads to every page after it.
    carry: VecDeque<RecordBatch>,
    /// Rows across `carry`, kept rather than summed on every call.
    held: usize,
    batch_rows: usize,
    delivered: u64,
    /// Set once every chunk has been read, which is not the same as having
    /// nothing left to hand over.
    drained: bool,
    _registration: Option<Registration>,
}

impl Rows {
    async fn open(
        wire: Arc<Wire>,
        sql: &str,
        batch_rows: usize,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, DatabricksError> {
        let started = wire.post(sql, Delivery::Arrow).await?;
        let id = started.statement_id.clone().ok_or_else(|| {
            DatabricksError::Transport(
                "the workspace started a statement without naming it, so nothing here could \
                 read it or stop it"
                    .to_string(),
            )
        })?;
        // Held from here rather than from the first successful page, so that a
        // statement cancelled while its warehouse is still starting up is one
        // `cancel` can name.
        let registration = register.map(|(live, at)| Registration::hold(live, at, id.clone()));

        let finished = settle(&wire, started).await?;
        check_format(
            finished.manifest.as_ref().and_then(|m| m.format.as_deref()),
            Delivery::Arrow,
        )?;

        let chunk = finished.result.unwrap_or_default();
        let next_chunk = chunk.next().map(str::to_string);
        let links: VecDeque<String> = chunk
            .external_links
            .into_iter()
            .map(|link| link.external_link)
            .collect();

        let mut rows = Rows {
            wire,
            statement: id,
            // Replaced by the first chunk's own schema. A statement with no
            // result set at all — a `CREATE TABLE` — has no chunk to take one
            // from and keeps this, which is the honest shape for it.
            schema: Arc::new(Schema::empty()),
            links,
            next_chunk,
            carry: VecDeque::new(),
            held: 0,
            batch_rows,
            delivered: 0,
            drained: false,
            _registration: registration,
        };

        // The first chunk is read here rather than left for `next_page`, because
        // `query` promises the columns before any row is handed over and the only
        // description of them this driver will accept is the Arrow schema itself.
        rows.pull().await?;
        Ok(rows)
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows this statement produced, or `None` until the result has been read to
    /// the end.
    ///
    /// Rows produced, and not rows changed — the same answer the Flight SQL
    /// driver gives. Databricks reports a write as a one-row result holding the
    /// count, so an `INSERT` of five rows says 1, being the one row the grid is
    /// about to show, and the number the user wants is in it.
    pub fn rows_affected(&self) -> Option<u64> {
        (self.drained && self.carry.is_empty()).then_some(self.delivered)
    }

    /// The next page, or `None` once the result is fully consumed.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, DatabricksError> {
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
            self.pull().await?;
        }
    }

    /// Reads one chunk forward, or marks the result drained.
    ///
    /// This is the whole of the decode, and the shape of it is the point. The
    /// body arrives as an owned `bytes::Bytes` and `Buffer::from` takes it whole,
    /// so the arrays built out of it are windows into the bytes hyper read off
    /// the socket — where a decoder that only ever saw a `&[u8]` would copy the
    /// lot. The Flight SQL driver measured what that is worth against a real
    /// server; this one cannot, and does not claim it.
    ///
    /// A decoder per chunk, because that is what makes chunks independently
    /// fetchable: each presigned link is its own Arrow stream, schema message
    /// included. `finish` at the end is what turns a truncated body into a
    /// sentence rather than a short result nobody notices.
    async fn pull(&mut self) -> Result<(), DatabricksError> {
        let Some(url) = self.next_link().await? else {
            self.drained = true;
            return Ok(());
        };

        let mut buffer = Buffer::from(self.wire.fetch(&url).await?);
        let mut decoder = StreamDecoder::new();
        while !buffer.is_empty() {
            // An empty batch is dropped rather than queued: a chunk that carried
            // one would otherwise sit in the queue holding no rows, and `take`
            // would be asked to slice nothing off the front of it.
            if let Some(batch) = decoder.decode(&mut buffer)?
                && batch.num_rows() > 0
            {
                self.held += batch.num_rows();
                self.carry.push_back(batch);
            }
        }
        decoder.finish()?;

        if let Some(schema) = decoder.schema() {
            // Every chunk of one result describes the same columns, and a caller
            // was promised the schema before the first row. A later chunk that
            // disagreed would silently change the grid under the rows already in
            // it.
            if self.schema.fields().is_empty() {
                self.schema = schema;
            } else if self.schema != schema {
                return Err(DatabricksError::Transport(
                    "the chunks of this result do not agree about its columns".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// The next presigned link, asking the API for more when the known ones run
    /// out.
    async fn next_link(&mut self) -> Result<Option<String>, DatabricksError> {
        loop {
            if let Some(url) = self.links.pop_front() {
                return Ok(Some(url));
            }
            let Some(next) = self.next_chunk.take() else {
                return Ok(None);
            };
            let chunk = self.wire.chunk(&next).await?;
            self.next_chunk = chunk.next().map(str::to_string);
            self.links = chunk
                .external_links
                .into_iter()
                .map(|link| link.external_link)
                .collect();
        }
    }

    /// Splits `rows` off the front of the queue.
    ///
    /// The whole page comes out of the front batch wherever it fits there, and
    /// then it is a slice: the page and the remainder go on pointing at the same
    /// buffers, so what the caller holds is still the bytes that were read. Only
    /// a page that straddles a boundary is concatenated.
    fn take(&mut self, rows: usize) -> Result<RecordBatch, DatabricksError> {
        self.delivered += rows as u64;
        self.held -= rows;

        if self.carry.front().is_some_and(|b| b.num_rows() >= rows) {
            let front = self.carry.front_mut().expect("just looked");
            let page = front.slice(0, rows);
            if front.num_rows() == rows {
                self.carry.pop_front();
            } else {
                *front = front.slice(rows, front.num_rows() - rows);
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
            let taken = want.min(front.num_rows());
            parts.push(front.slice(0, taken));
            if front.num_rows() == taken {
                self.carry.pop_front();
            } else {
                *front = front.slice(taken, front.num_rows() - taken);
            }
            want -= taken;
        }
        Ok(arrow::compute::concat_batches(&self.schema, &parts)?)
    }

    /// A handle for stopping this reader from another thread.
    ///
    /// Taken out in advance rather than reached for at cancel time, because by
    /// then the reader is borrowed by the fetch that is to be stopped. The
    /// statement id it names was chosen by the workspace before the first chunk
    /// existed and does not move.
    pub fn canceller(&self) -> RowsCancel {
        RowsCancel {
            wire: Arc::clone(&self.wire),
            statement: self.statement.clone(),
        }
    }

    /// Lets go of whatever is held.
    ///
    /// Optional; dropping does the same. Note what neither of them does: the
    /// warehouse is not told. A finished result stays fetchable for as long as the
    /// API keeps it, so there is nothing to release — stopping a statement that is
    /// still *running* is what `canceller` is for.
    pub async fn close(&mut self) -> Result<(), DatabricksError> {
        self.links.clear();
        self.next_chunk = None;
        self.carry.clear();
        self.held = 0;
        self.drained = true;
        self._registration = None;
        Ok(())
    }
}

/// Stops the statement one reader is running.
#[derive(Clone)]
pub struct RowsCancel {
    wire: Arc<Wire>,
    statement: String,
}

impl RowsCancel {
    /// Delivered is not interrupted. A statement that had already finished leaves
    /// nothing to stop and this still succeeds; what actually happened shows up as
    /// the reader failing with `is_cancelled`, or not failing at all.
    pub async fn cancel(&self) -> Result<(), DatabricksError> {
        self.wire.cancel(&self.statement).await
    }
}

/// The bytes of one Arrow chunk as the batches they hold.
///
/// Free rather than a method, so the tests can reach it without a workspace. It
/// is the same decode `Rows::pull` performs; that one keeps the schema and this
/// one hands it back.
#[cfg(test)]
fn decode(bytes: Bytes) -> Result<(Option<SchemaRef>, Vec<RecordBatch>), DatabricksError> {
    let mut buffer = Buffer::from(bytes);
    let mut decoder = StreamDecoder::new();
    let mut batches = Vec::new();
    while !buffer.is_empty() {
        if let Some(batch) = decoder.decode(&mut buffer)? {
            batches.push(batch);
        }
    }
    decoder.finish()?;
    Ok((decoder.schema(), batches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field};

    /// Needs no server — it needs the absence of one. Port 1 is reserved and
    /// nothing on a developer machine or a CI runner listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let error = DatabricksSource::connect(
            "https://127.0.0.1:1/main/default?warehouse_id=abc&token=dapi",
        )
        .await
        .err()
        .expect("nothing is listening on port 1");
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains("refused") || message.contains("connect"),
            "the refusal should survive into the message, got: {message}"
        );
    }

    /// The warehouse id is the one part of a Databricks connection string that
    /// has no default anywhere, so its absence is said here rather than by the
    /// API — whose answer to it is a 400 about a missing field.
    #[tokio::test]
    async fn a_connection_with_no_warehouse_says_where_to_find_one() {
        let message = DatabricksSource::connect("https://host/main?token=dapi")
            .await
            .err()
            .expect("no warehouse")
            .to_string();
        assert!(message.contains("warehouse_id"), "got: {message}");
    }

    #[tokio::test]
    async fn a_connection_with_no_credentials_says_which_ones_it_wants() {
        let message = DatabricksSource::connect("https://host/main?warehouse_id=abc")
            .await
            .err()
            .expect("no credentials")
            .to_string();
        assert!(message.contains("token"), "got: {message}");
        assert!(message.contains("client_id"), "got: {message}");
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_before_anything_is_sent() {
        let error = DatabricksSource::connect("not a url at all")
            .await
            .err()
            .unwrap();
        assert!(matches!(error, DatabricksError::BadUrl(_)));
    }

    /// A catalog cannot be reached without naming it, and a schema string that
    /// never came from `schemas()` splits into nothing rather than into a guess.
    #[test]
    fn a_composite_schema_splits_at_the_catalog_and_not_at_the_last_dot() {
        assert_eq!(parts("main.default"), Some(("main", "default")));
        assert_eq!(parts("main.year.2024"), Some(("main", "year.2024")));
        assert_eq!(parts("default"), None);
        assert_eq!(parts(""), None);
    }

    /// The two ways this driver writes a value into a statement it composes.
    /// Databricks reads a backslash inside a string literal as an escape, which
    /// is the one place its quoting differs from Trino's and Snowflake's — a path
    /// ending in a backslash would otherwise swallow the closing quote.
    #[test]
    fn a_name_or_a_value_with_a_delimiter_in_it_survives_being_written_down() {
        assert_eq!(quote("we`ird"), "`we``ird`");
        assert_eq!(quote("orders"), "`orders`");
        assert_eq!(literal("O'Brien"), "'O''Brien'");
        assert_eq!(literal(r"c:\temp\"), r"'c:\\temp\\'");
    }

    /// A result delivered in the other form is refused with a sentence, rather
    /// than reaching the Arrow decoder and failing with something about a
    /// continuation marker.
    #[test]
    fn a_result_in_the_wrong_encoding_says_so_before_it_is_decoded() {
        assert!(check_format(Some("ARROW_STREAM"), Delivery::Arrow).is_ok());
        assert!(check_format(Some("JSON_ARRAY"), Delivery::Inline).is_ok());
        assert!(check_format(None, Delivery::Arrow).is_ok());
        let message = check_format(Some("JSON_ARRAY"), Delivery::Arrow)
            .expect_err("the wrong encoding")
            .to_string();
        assert!(message.contains("ARROW_STREAM"), "got: {message}");
    }

    /// One chunk of the format this driver is here for, read back the way
    /// `Rows::pull` reads one.
    ///
    /// This is not a server pretending to answer: the bytes are written by
    /// Arrow's own IPC writer in this process, and what it establishes is only
    /// that the decode is the right decode for an Arrow stream — schema first,
    /// batches after, end of stream at the end. Whether Databricks writes one of
    /// these is exactly what nobody here can say.
    #[test]
    fn an_arrow_stream_is_read_back_as_the_columns_it_describes() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("label", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
            ],
        )
        .expect("a batch");

        let mut written = Vec::new();
        {
            let mut writer =
                arrow::ipc::writer::StreamWriter::try_new(&mut written, &schema).expect("a writer");
            writer.write(&batch).expect("a batch written");
            writer.finish().expect("the stream finished");
        }

        let (read_schema, batches) = decode(Bytes::from(written)).expect("a stream");
        assert_eq!(read_schema.as_deref(), Some(schema.as_ref()));
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        assert!(batches[0].column(1).is_null(1));
    }

    /// A body that stops in the middle is a sentence rather than a short result.
    ///
    /// The failure this guards against is the quiet one: a presigned link that
    /// expires mid-transfer, or a connection dropped between two batches, would
    /// otherwise look exactly like a table with fewer rows in it than it has.
    #[test]
    fn a_truncated_chunk_is_a_failure_and_not_a_short_result() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int64Array::from(vec![1, 2, 3]))],
        )
        .expect("a batch");
        let mut written = Vec::new();
        {
            let mut writer =
                arrow::ipc::writer::StreamWriter::try_new(&mut written, &schema).expect("a writer");
            writer.write(&batch).expect("a batch written");
            writer.finish().expect("the stream finished");
        }
        written.truncate(written.len() / 2);
        assert!(decode(Bytes::from(written)).is_err());
    }
}
