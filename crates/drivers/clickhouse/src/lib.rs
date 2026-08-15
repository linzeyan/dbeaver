//! ClickHouse, read over the HTTP interface as Arrow.
//!
//! The transport is HTTP and not the native TCP protocol, which reverses what
//! `docs/drivers.md` recorded. Three findings decided it, each checked against
//! `klickhouse` 0.15.3's own source rather than inherited:
//!
//! - Its `Type::from_str` has no arm for `Date32`, `JSON`, `Variant`, `Dynamic`
//!   or `Time`, refuses `Nested` by name, and aliases `Bool` to `UInt8`. A
//!   `SELECT` touching one of those columns fails at type-parse time, before a
//!   row moves — a client whose failure mode is "this table has a column type I
//!   decline to name" is not a client.
//! - `ClientPacketId::Cancel` occurs exactly once in the crate, as an unused
//!   enum variant, and the query id is generated inside `dispatch_query` and
//!   never surfaced (there is a `TODO` at `client.rs:552` saying so). There is
//!   no cancellation to build a Cancel button on.
//! - Every cell is a `klickhouse::Value`, 36 variants wide, one discriminant and
//!   one allocation per string — and reaching Arrow from there is the
//!   per-column builder machinery this driver does not have to write at all.
//!
//! What HTTP costs is session state, and this driver deliberately does not buy
//! it back: no `session_id` is set, so every request stands alone. That is what
//! makes `KILL QUERY` an ordinary second request instead of the workaround the
//! upstream Java plugin needs — a busy session refuses a second statement with
//! `Code: 373`, and `ClickhouseDataSource.fallbackForServerID` opens a fresh
//! connection to get around it.
//!
//! **A cursor and a query are the same call here.** ClickHouse has no `DECLARE
//! CURSOR`, no `FETCH`, and no server-side handle to come back to. What it has
//! is the property a cursor exists for: a `SELECT` fixes the set of parts it
//! will read when it starts, so inserts and merges landing afterwards are not in
//! it, and the client pulls the result forward one block at a time. Not calling
//! `next` stops reading the socket, TCP's window closes and the server stops
//! producing, so the backpressure the PostgreSQL driver gets from a bounded
//! channel falls out of the transport. `query` and `cursor` therefore return the
//! same type, which is a finding and not a shortcut.
//!
//! One caveat with teeth: dropping the reader does **not** stop the statement.
//! `cancel_http_readonly_queries_on_client_close` defaults to 0 and covers only
//! readonly queries, so `Drop` is not a cancellation and this driver never
//! pretends it is.

mod arrow_map;
mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow_map::Plan;
use clickhouse::Client;
use clickhouse_ext_arrow::{ArrowCursor, ArrowQueryExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub use metadata::Storage;

/// ClickHouse's own code for a statement somebody stopped.
///
/// `TIMEOUT_EXCEEDED` is 159 and is a different thing that must not be reported
/// as a cancellation — the server draws the line deliberately, and so does this.
const QUERY_WAS_CANCELLED: i32 = 394;

#[derive(Debug, thiserror::Error)]
pub enum ChError {
    /// A request that did not work, with the two facts a front end acts on
    /// already read out of the server's exception before the rest of it became
    /// a string.
    #[error("{message}")]
    Request {
        message: String,
        /// ClickHouse's own error code, where the failure reached the server at
        /// all.
        code: Option<i32>,
        /// 1-based, counted in characters, into the text the caller wrote.
        position: Option<u32>,
    },
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl ChError {
    /// Reads a `clickhouse::Error` for the facts a front end needs, resolving
    /// any statement offset against `sent`.
    ///
    /// `sent` must be the text the caller wrote, or `None`. This driver rewrites
    /// statements — it wraps them in a projection, and it asks `DESCRIBE` about
    /// them first — and an offset into text the user never saw would put the
    /// caret in the wrong place with complete confidence. No position is better
    /// than a wrong one.
    fn from_server(error: clickhouse::error::Error, sent: Option<&str>) -> Self {
        let message = with_causes(&error);
        let exception = match &error {
            clickhouse::error::Error::BadResponse(text) => Some(text.as_str()),
            _ => None,
        };
        ChError::Request {
            code: exception.and_then(server_code),
            position: sent.zip(exception).and_then(|(sql, e)| position(e, sql)),
            message,
        }
    }

    /// Whether the server stopped this statement because somebody asked it to.
    ///
    /// Read from the code the server sent rather than from this side
    /// remembering that it pressed Cancel: a statement can fail on its own
    /// merits in the same moment the `KILL QUERY` lands, and reporting that as
    /// cancelled hides a real fault behind a button.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, ChError::Request { code, .. } if *code == Some(QUERY_WAS_CANCELLED))
    }

    /// Where in the statement the server says the trouble is: 1-based, counted
    /// in characters.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            ChError::Request { position, .. } => *position,
            _ => None,
        }
    }
}

/// Renders a failure together with what caused it.
///
/// A connection that never happened carries no ClickHouse exception, and what
/// the crate displays for one names the layer rather than the cause: "network
/// error: client error (Connect)" fits every connection failure there is —
/// wrong port, no route, no server, TLS refused — and a connection dialog
/// showing it leaves the user to guess which. The reason is further down the
/// source chain, so the chain is what gets rendered.
fn with_causes(error: &clickhouse::error::Error) -> String {
    use std::error::Error;
    let mut out = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        let text = next.to_string();
        // The crate's own variants interpolate their source into their Display,
        // so following the chain blindly repeats it.
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cause = next.source();
    }
    out
}

/// The numeric code out of a ClickHouse exception.
///
/// Parsed out of the message because `clickhouse::error::Error` has no variant
/// that carries it: `BadResponse` holds the server's exception verbatim, and
/// every one of them opens `Code: <n>. DB::Exception: …`. That prefix is the
/// only structured thing in the string.
///
/// This is weaker than what the other two drivers have — PostgreSQL reads a
/// SQLSTATE and SQLite an `ErrorCode` — and it is worth saying so here rather
/// than letting a reader assume they are equivalent. Keyed on the number and
/// never on the name, because the number is the identifier and the name is
/// prose. The response headers carry the code too
/// (`X-ClickHouse-Exception-Code`), but the crate does not expose them, and
/// reaching them means decoding the Arrow IPC stream by hand.
fn server_code(exception: &str) -> Option<i32> {
    exception
        .strip_prefix("Code: ")?
        .split_once('.')?
        .0
        .trim()
        .parse()
        .ok()
}

/// The statement offset out of a ClickHouse syntax error, converted to the
/// contract the trait states.
///
/// ClickHouse says `Syntax error: failed at position 63 ('BY')`, and the number
/// is a **byte** offset counted from one. The trait wants characters counted
/// from one, and the difference is invisible in English: the same statement with
/// a CJK identifier in it reports 63 where the character is the 51st. So the
/// prefix is measured in bytes and re-counted in characters.
fn position(exception: &str, sql: &str) -> Option<u32> {
    let at = exception.find("failed at position ")? + "failed at position ".len();
    let digits: String = exception[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let bytes: usize = digits.parse().ok()?;
    // Past the end means the offset is not describing this statement, which
    // happens whenever the caller's text was not what the server was sent.
    if bytes == 0 || bytes > sql.len() + 1 {
        return None;
    }
    let characters = sql
        .char_indices()
        .take_while(|(offset, _)| *offset < bytes - 1)
        .count();
    Some(characters as u32 + 1)
}

/// The registry of statements this session has in flight, by query id.
///
/// A `std::sync::Mutex` and not tokio's, because a reader removes its own entry
/// from `Drop`, which cannot await.
type Live = Arc<Mutex<HashMap<u64, String>>>;

/// One session against one ClickHouse server.
///
/// There is no connection to hold. HTTP is stateless, `clickhouse::Client` is a
/// pooled hyper client behind an `Arc`, and cloning it is how a second request
/// happens — which is why `cancel` needs no separate connection and no pool.
pub struct ChSource {
    client: Client,
    database: String,
    /// What a `DateTime` with no zone of its own means on this server.
    ///
    /// Read once at connect rather than assumed to be UTC. ClickHouse puts the
    /// zone in the Arrow field, so a driver that guessed would label every
    /// timestamp on a server configured for another zone wrongly, and there is
    /// no second place for the front end to check.
    timezone: String,
    live: Live,
    next: AtomicU64,
}

impl ChSource {
    /// Connects to `url`, of the form
    /// `http://user:password@host:port/database`.
    ///
    /// `https` works and is what ClickHouse Cloud needs. The database in the
    /// path is the one unqualified names resolve in; `default` where the path is
    /// empty, as the server itself would.
    pub async fn connect(url: &str) -> Result<Self, ChError> {
        let parsed = url::Url::parse(url).map_err(|e| ChError::BadUrl(format!("{url}: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| ChError::BadUrl(format!("{url}: no host")))?;
        let origin = match parsed.port() {
            Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
            None => format!("{}://{host}", parsed.scheme()),
        };
        let database = match parsed.path().trim_start_matches('/') {
            "" => "default".to_string(),
            name => percent_decode(name),
        };

        let mut client = Client::default().with_url(origin).with_database(&database);
        if !parsed.username().is_empty() {
            client = client.with_user(percent_decode(parsed.username()));
        }
        if let Some(password) = parsed.password() {
            client = client.with_password(percent_decode(password));
        }

        // One round trip that both proves the credentials work and answers a
        // question the type mapping cannot do without. A driver whose `connect`
        // succeeds against a wrong password, and fails at the first query
        // instead, moves the error away from the dialog that caused it.
        let timezone: String = client
            .query("SELECT timezone()")
            .fetch_one()
            .await
            .map_err(|e| ChError::from_server(e, None))?;

        Ok(Self {
            client,
            database,
            timezone,
            live: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
        })
    }

    /// The database unqualified names resolve in.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// Runs `sql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the columns are known and before any row has been read, so
    /// a caller can lay out a grid immediately. That costs one round trip:
    /// `DESCRIBE (<sql>)` plans the statement without executing it, and it is
    /// the only way to have the columns in advance — `ArrowCursor::schema()`
    /// answers `None` until the first batch has arrived, which is too late for
    /// the contract this promises. The same round trip is where the declared
    /// ClickHouse types come from, which the Arrow schema has already thrown
    /// away and which `arrow_map` cannot work without, so it is not an
    /// optimisation that could be dropped.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<Rows, ChError> {
        self.read(sql, batch_rows).await
    }

    /// Reads `sql` forward, a page at a time.
    ///
    /// The same call as `query`, for the reason in the module comment: one open
    /// response body already is a snapshot read forward without re-reading, and
    /// there is no second mechanism to reach for.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Rows, ChError> {
        self.read(sql, batch_rows).await
    }

    async fn read(&self, sql: &str, batch_rows: usize) -> Result<Rows, ChError> {
        let query_id = uuid::Uuid::new_v4().to_string();
        let attempt = Attempt {
            client: self.client.clone(),
            original: sql.to_string(),
            columns: Vec::new(),
            timezone: self.timezone.clone(),
            batch_rows: batch_rows.max(1),
            query_id: query_id.clone(),
        };

        let columns = match self.describe(sql).await {
            Ok(columns) => columns,
            // Not everything that can be run can be described: `DESCRIBE (INSERT
            // …)` and `DESCRIBE (SHOW …)` are syntax errors, and so is
            // `DESCRIBE` of a statement that is simply broken. What separates
            // those is whether the statement answers with rows — an `INSERT`
            // does not, a `SHOW` does — and that decides how it has to be sent,
            // because reading a result means asking for `FORMAT ArrowStream`
            // and on an `INSERT` that names the format of the data going in.
            Err(_) => {
                let attempt = Attempt {
                    columns: Vec::new(),
                    ..attempt
                };
                if !answers_with_rows(sql) {
                    // Running the caller's own text, which reports a broken
                    // statement with an offset into what they actually wrote and
                    // carries out one that has no result set.
                    attempt.execute().await?;
                    return Ok(Rows::finished(attempt.plan(false).schema));
                }
                let registration =
                    Registration::hold(Arc::clone(&self.live), self.next(), query_id);
                return Rows::undescribed(attempt, registration).await;
            }
        };

        let attempt = Attempt { columns, ..attempt };
        let registration = Registration::hold(Arc::clone(&self.live), self.next(), query_id);
        Rows::open(attempt, registration).await
    }

    /// The columns a statement will produce, and the types ClickHouse declares
    /// them with.
    ///
    /// Errors are deliberately not decorated with a position: the offset would
    /// be into `DESCRIBE (…)` and off by the length of that prefix, which is a
    /// caret confidently pointing at the wrong character.
    async fn describe(&self, sql: &str) -> Result<Vec<(String, String)>, ChError> {
        let mut cursor = self
            .client
            .query(&format!("DESCRIBE ({sql})"))
            .fetch_arrow()
            .map_err(|e| ChError::from_server(e, None))?;

        let mut columns = Vec::new();
        while let Some(batch) = cursor
            .next()
            .await
            .map_err(|e| ChError::from_server(e, None))?
        {
            let names = text_column(&batch, 0)?;
            let types = text_column(&batch, 1)?;
            for row in 0..batch.num_rows() {
                columns.push((names.value(row).to_string(), types.value(row).to_string()));
            }
        }
        Ok(columns)
    }

    /// Asks the server to abandon whatever this session is running.
    ///
    /// One request naming every live statement, because HTTP is stateless and a
    /// `KILL QUERY` contends with nothing — the situation the PostgreSQL driver
    /// needs a second connection for does not arise.
    ///
    /// Best-effort, as everywhere else: success means the request was delivered,
    /// not that anything stopped. A session with nothing running does not send
    /// one at all, because cancelling an idle session is a no-op and a round
    /// trip that proves it is a round trip wasted.
    pub async fn cancel(&self) -> Result<(), ChError> {
        let ids: Vec<String> = match self.live.lock() {
            Ok(live) => live.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        if ids.is_empty() {
            return Ok(());
        }
        kill(&self.client, &ids).await
    }

    /// The ClickHouse-only facts about a table, which `RelationInfo` has no room
    /// for.
    ///
    /// A tenth call rather than four fields nobody else has. The engine is the
    /// single most consequential thing about a ClickHouse table — it decides
    /// whether the row count is real, whether the data is even on this server,
    /// and what may be done to it — and the sorting key is the closest thing it
    /// has to an index, so dropping both on the way through the trait would lose
    /// what a structure pane most needs to say.
    pub async fn storage(&self, schema: &str, relation: &str) -> Result<Option<Storage>, ChError> {
        metadata::storage(&self.client, schema, relation).await
    }

    fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// `KILL QUERY` for a set of query ids, synchronously.
///
/// `SYNC` rather than the default `ASYNC`: it waits and reports a `kill_status`,
/// which turns "the request was delivered" into "the statement is gone". Naming
/// a query that is not running is not an error — the statement matches nothing
/// and the request succeeds — which is exactly what cancelling an idle cursor
/// has to do.
async fn kill(client: &Client, ids: &[String]) -> Result<(), ChError> {
    client
        .query("KILL QUERY WHERE query_id IN ? SYNC")
        .bind(ids)
        .execute()
        .await
        .map_err(|e| ChError::from_server(e, None))
}

/// Everything needed to start — or restart — one statement's stream.
///
/// Held rather than consumed because the invalid-UTF-8 retry re-issues the same
/// statement asked a slightly different way, and it has to be able to.
struct Attempt {
    client: Client,
    /// The text the caller wrote, kept so a failure's offset can be resolved
    /// against it and so the retry re-wraps the same thing.
    original: String,
    columns: Vec<(String, String)>,
    timezone: String,
    batch_rows: usize,
    query_id: String,
}

impl Attempt {
    fn plan(&self, sanitize: bool) -> Plan {
        arrow_map::plan(&self.columns, &self.timezone, sanitize)
    }

    /// The statement as it will be sent.
    ///
    /// The caller's text untouched wherever it can be, because wrapping it in a
    /// projection is not free of consequences: ClickHouse collapses two output
    /// columns that share a name, so `SELECT a, a FROM t` would come back one
    /// column narrower than the schema promised. When the names are not unique
    /// the statement goes out as written and a type the Arrow writer refuses
    /// fails with the server's own message, which is the honest outcome.
    fn statement(&self, plan: &Plan) -> String {
        let unique = {
            let mut names: Vec<&str> = self.columns.iter().map(|(n, _)| n.as_str()).collect();
            names.sort_unstable();
            names.windows(2).all(|pair| pair[0] != pair[1])
        };
        match &plan.select_list {
            Some(list) if unique => format!("SELECT {list} FROM ({})", self.original),
            _ => self.original.clone(),
        }
    }

    fn open(&self, sanitize: bool) -> Result<(ArrowCursor, Plan), ChError> {
        let plan = self.plan(sanitize);
        let cursor = self
            .settings(self.client.query(&self.statement(&plan)))
            .fetch_arrow()
            .map_err(|e| ChError::from_server(e, Some(&self.original)))?;
        Ok((cursor, plan))
    }

    async fn execute(&self) -> Result<(), ChError> {
        self.settings(self.client.query(&self.original))
            .execute()
            .await
            .map_err(|e| ChError::from_server(e, Some(&self.original)))
    }

    /// Every setting the Arrow output depends on, stated rather than inherited.
    ///
    /// A server-side default that moves under us silently changes a column's
    /// Arrow type, and a driver that reads whatever it is handed cannot tell
    /// that from a schema change — it would just start failing to build
    /// batches. `output_format_arrow_compression_method` is missing on purpose:
    /// `clickhouse-ext-arrow` forces it to `none` itself, and setting it again
    /// would only be a second place to get it wrong.
    fn settings(&self, query: clickhouse::query::Query) -> clickhouse::query::Query {
        query
            // Text is text. The alternative renders every string column as hex,
            // which is a worse answer to a problem most databases do not have.
            .with_setting("output_format_arrow_string_as_string", "1")
            // `FixedString(n)` would otherwise arrive as `FixedSizeBinary(n)`,
            // which the reader on the far side of the FFI has no case for;
            // `Binary` it can open. `arrow_map` states the same fact and this is
            // what makes it true.
            .with_setting("output_format_arrow_fixed_string_as_fixed_byte_array", "0")
            // A dictionary is the right Arrow representation of a
            // `LowCardinality` column and the wrong one to send: the C Data
            // Interface carries it in a field the Swift reader never looks at,
            // so every cell would draw as the index type's format string.
            .with_setting("output_format_arrow_low_cardinality_as_dictionary", "0")
            // The page size, set on the server's pipeline rather than asked for
            // a page at a time. This is the knob `FETCH FORWARD n` is standing
            // in for in the PostgreSQL driver, and it is a better one.
            .with_setting("max_block_size", self.batch_rows.to_string())
            // Reserved URL parameter rather than a setting, but it travels the
            // same way, and it has to be ours: the cancel handle exists before
            // the statement does.
            .with_setting("query_id", self.query_id.clone())
    }
}

/// Puts a statement's query id where `ChSource::cancel` can find it, and takes
/// it back out when the reader is dropped.
///
/// A query id whose statement has finished is not something to leave lying in a
/// list — `KILL QUERY` would name it, match nothing, and cost a round trip
/// proving so.
struct Registration {
    id: u64,
    live: Live,
}

impl Registration {
    fn hold(live: Live, id: u64, query_id: String) -> Self {
        if let Ok(mut held) = live.lock() {
            held.insert(id, query_id);
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
/// Both a `ResultStream` and a `Cursor`, because against ClickHouse they are the
/// same thing.
pub struct Rows {
    schema: SchemaRef,
    reader: Option<Reader>,
    /// Rows read out of the response but not yet handed over.
    ///
    /// `max_block_size` is a hint to the server's pipeline and not a promise, so
    /// a block can arrive short — a caller that asked for pages of 50 and was
    /// given 47 would see a page boundary where there is none in the data. The
    /// carry is what makes the page size the caller's number rather than the
    /// server's.
    carry: Option<RecordBatch>,
    delivered: u64,
    /// Set once the response body has ended, which is not the same as having
    /// nothing left to hand over.
    drained: bool,
    /// Whether anything has been given to the caller yet, which is what decides
    /// whether the invalid-UTF-8 retry is still possible.
    untouched: bool,
    /// Whether the rows handed over are a count of anything.
    ///
    /// False for a statement that had no result set: it did something, and how
    /// much is a number this driver does not have.
    counted: bool,
    _registration: Option<Registration>,
}

struct Reader {
    cursor: ArrowCursor,
    attempt: Attempt,
    sanitized: bool,
}

impl Rows {
    async fn open(attempt: Attempt, registration: Registration) -> Result<Self, ChError> {
        let (cursor, plan) = attempt.open(false)?;
        Ok(Self {
            schema: plan.schema,
            reader: Some(Reader {
                cursor,
                attempt,
                sanitized: false,
            }),
            carry: None,
            delivered: 0,
            drained: false,
            untouched: true,
            counted: true,
            _registration: Some(registration),
        })
    }

    /// A result whose columns exist only once the statement has run.
    ///
    /// The first block is read here rather than left for `next_page`, because
    /// the schema is in it and `query` promises the columns before any row is
    /// handed over. That promise is kept for these statements too — just at the
    /// cost of the round trip `DESCRIBE` would have made anyway.
    ///
    /// The invalid-UTF-8 retry survives this: nothing has been delivered, so a
    /// re-run repeats no page. What it cannot do is help — sanitizing works by
    /// wrapping the columns `DESCRIBE` named, and there are none here.
    async fn undescribed(attempt: Attempt, registration: Registration) -> Result<Self, ChError> {
        let (mut cursor, plan) = attempt.open(false)?;
        let first = cursor
            .next()
            .await
            .map_err(|e| ChError::from_server(e, Some(&attempt.original)))?;
        let Some(batch) = first else {
            // It ran and answered with nothing at all, which is the shape of a
            // statement that had no result set after all.
            return Ok(Self::finished(plan.schema));
        };
        Ok(Self {
            schema: batch.schema(),
            reader: Some(Reader {
                cursor,
                attempt,
                sanitized: false,
            }),
            carry: Some(batch),
            delivered: 0,
            drained: false,
            untouched: true,
            counted: true,
            _registration: Some(registration),
        })
    }

    /// A result with no rows in it and none coming, for a statement that had no
    /// result set to begin with.
    fn finished(schema: SchemaRef) -> Self {
        Self {
            schema,
            reader: None,
            carry: None,
            delivered: 0,
            drained: true,
            untouched: false,
            counted: false,
            _registration: None,
        }
    }

    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows this statement produced, or `None` until the result has been read to
    /// the end.
    ///
    /// Counted here rather than taken from the server, which is exact for a
    /// statement that returns rows and unavailable for one that does not.
    /// ClickHouse reports what a statement read and wrote in an
    /// `X-ClickHouse-Summary` response header and the crate does not surface
    /// it, so an `INSERT` that wrote a thousand rows has no number to give.
    ///
    /// It answers `None` rather than `0`. That is the one ambiguity in this
    /// method — the trait uses `None` for "not read to the end", and here it
    /// also has to mean "never knowable" — but the alternative is stating that
    /// an insert of a million rows affected none, and a wrong number is worse
    /// than a missing one.
    pub fn rows_affected(&self) -> Option<u64> {
        (self.counted && self.drained && self.carry.is_none()).then_some(self.delivered)
    }

    /// The next page, or `None` once the result is fully consumed.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, ChError> {
        let page = self.fill().await?;
        if let Some(batch) = &page {
            self.delivered += batch.num_rows() as u64;
            self.untouched = false;
        }
        Ok(page)
    }

    async fn fill(&mut self) -> Result<Option<RecordBatch>, ChError> {
        loop {
            let held = self.carry.as_ref().map_or(0, RecordBatch::num_rows);
            if held >= self.batch_rows() {
                return Ok(Some(self.take(self.batch_rows())));
            }
            if self.drained {
                return Ok((held > 0).then(|| self.take(held)));
            }
            match self.pull().await? {
                Some(batch) => {
                    let batch =
                        RecordBatch::try_new(Arc::clone(&self.schema), batch.columns().to_vec())?;
                    self.carry = match self.carry.take() {
                        None => Some(batch),
                        Some(held) => Some(arrow::compute::concat_batches(
                            &self.schema,
                            &[held, batch],
                        )?),
                    };
                }
                None => self.drained = true,
            }
        }
    }

    /// One block from the response, retrying once through `toValidUTF8` if the
    /// text in it turns out not to be text.
    ///
    /// ClickHouse's `String` holds arbitrary bytes and the server does not check
    /// them, so an invalid sequence fails inside the Arrow IPC decoder and takes
    /// the whole result with it — one bad row makes a table unbrowsable. The
    /// retry costs one wasted execution on a database with dirty strings and
    /// nothing at all on a clean one, which is the right way round.
    ///
    /// Only while nothing has been handed over. Past that the statement would
    /// have to be re-run from the beginning, and a page the caller has already
    /// seen would arrive again — which is the one thing a cursor promises not to
    /// do.
    async fn pull(&mut self) -> Result<Option<RecordBatch>, ChError> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        match reader.cursor.next().await {
            Ok(batch) => Ok(batch),
            Err(error) => {
                let recoverable = self.untouched && !reader.sanitized && is_invalid_utf8(&error);
                if !recoverable {
                    return Err(ChError::from_server(error, Some(&reader.attempt.original)));
                }
                let (cursor, _) = reader.attempt.open(true)?;
                reader.cursor = cursor;
                reader.sanitized = true;
                self.carry = None;
                reader
                    .cursor
                    .next()
                    .await
                    .map_err(|e| ChError::from_server(e, Some(&reader.attempt.original)))
            }
        }
    }

    fn batch_rows(&self) -> usize {
        self.reader.as_ref().map_or(1, |r| r.attempt.batch_rows)
    }

    /// Splits `rows` off the front of the carry.
    fn take(&mut self, rows: usize) -> RecordBatch {
        let held = self.carry.take().expect("take is only called with a carry");
        let page = held.slice(0, rows);
        let rest = held.slice(rows, held.num_rows() - rows);
        self.carry = (rest.num_rows() > 0).then_some(rest);
        page
    }

    /// A handle for stopping this reader from another thread.
    ///
    /// Taken out in advance rather than reached for at cancel time, because by
    /// then the reader is borrowed by the fetch that is to be stopped. The query
    /// id it names was chosen before the statement was sent, for the same
    /// reason.
    pub fn canceller(&self) -> RowsCancel {
        RowsCancel {
            client: self.reader.as_ref().map(|r| r.attempt.client.clone()),
            query_id: self.reader.as_ref().map(|r| r.attempt.query_id.clone()),
        }
    }

    /// Releases the response body.
    ///
    /// Optional; dropping does the same. Note what this does **not** do: closing
    /// the body does not stop the statement unless
    /// `cancel_http_readonly_queries_on_client_close` is on, and it is off by
    /// default, so a caller that wants the server to stop working has to say so
    /// through `canceller`.
    pub async fn close(&mut self) -> Result<(), ChError> {
        self.reader = None;
        self.carry = None;
        self.drained = true;
        self._registration = None;
        Ok(())
    }
}

/// Whether a statement ClickHouse will not describe still answers with rows.
///
/// Read off the leading keyword, which is as far as this has to look: the
/// statements the planner refuses to describe are the introspection ones and the
/// writes, and no member of either group hides its kind behind a prefix the way
/// a `WITH` hides a `SELECT` — and a `SELECT`, `WITH` or `VALUES` never reaches
/// here, because `DESCRIBE` answered for it.
///
/// Listed by what returns rows rather than by what does not, so a statement this
/// has never heard of keeps today's behaviour instead of being sent for a result
/// it may not have. `EXISTS` and `CHECK TABLE` are here for completeness; `SHOW`
/// is the one that matters, being how every DDL in this build is read.
fn answers_with_rows(sql: &str) -> bool {
    let word: String = sql
        .trim_start()
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    matches!(
        word.to_ascii_uppercase().as_str(),
        "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "EXISTS" | "CHECK"
    )
}

/// Stops the statement one reader is running.
pub struct RowsCancel {
    client: Option<Client>,
    query_id: Option<String>,
}

impl RowsCancel {
    /// Delivered is not interrupted. A statement that had already finished
    /// leaves nothing to stop and this still succeeds; what actually happened
    /// shows up as the reader failing with `is_cancelled`, or not failing at
    /// all.
    pub async fn cancel(&self) -> Result<(), ChError> {
        let (Some(client), Some(query_id)) = (&self.client, &self.query_id) else {
            return Ok(());
        };
        kill(client, std::slice::from_ref(query_id)).await
    }
}

/// Whether a failure is the Arrow decoder refusing a string column's bytes.
///
/// `clickhouse-ext-arrow` wraps every `ArrowError` as `Error::Other`, so the
/// variant alone does not say which one, and the text is what is left. Matched
/// narrowly on purpose: a retry keyed on `Error::Other` in general would re-run
/// statements that failed for reasons re-running cannot fix.
fn is_invalid_utf8(error: &clickhouse::error::Error) -> bool {
    matches!(error, clickhouse::error::Error::Other(_))
        && error.to_string().contains("Invalid UTF8")
}

/// One column of a `DESCRIBE` result, which is always text.
fn text_column(
    batch: &RecordBatch,
    at: usize,
) -> Result<&arrow::array::StringArray, arrow::error::ArrowError> {
    batch
        .column(at)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .ok_or_else(|| {
            arrow::error::ArrowError::SchemaError(format!(
                "DESCRIBE column {at} arrived as {:?}, not text",
                batch.column(at).data_type()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no server — it needs the absence of one. Port 1 is reserved and
    /// nothing on a developer machine or a CI runner listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let error = ChSource::connect("http://127.0.0.1:1/bench")
            .await
            .err()
            .expect("nothing is listening on port 1");
        let message = error.to_string();
        assert!(
            message.to_lowercase().contains("refused"),
            "the refusal should survive into the message, got: {message}"
        );
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_before_anything_is_sent() {
        let error = ChSource::connect("not a url at all").await.err().unwrap();
        assert!(matches!(error, ChError::BadUrl(_)));
    }

    /// The one place this driver is weaker than the other two, pinned so that a
    /// change in the crate's error text is a failing test rather than a Cancel
    /// button that silently reports faults.
    #[test]
    fn a_cancelled_statement_is_told_apart_from_a_slow_one() {
        let cancelled = ChError::from_server(
            clickhouse::error::Error::BadResponse(
                "Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED) \
                 (version 24.10.2.80 (official build))"
                    .to_string(),
            ),
            None,
        );
        assert!(cancelled.is_cancelled());

        // 159 is TIMEOUT_EXCEEDED, which the server raises from a few lines away
        // in the same file and which is not somebody pressing a button.
        let timed_out = ChError::from_server(
            clickhouse::error::Error::BadResponse(
                "Code: 159. DB::Exception: Timeout exceeded. (TIMEOUT_EXCEEDED)".to_string(),
            ),
            None,
        );
        assert!(!timed_out.is_cancelled());

        // A failure that never reached the server has no code and is not a
        // cancellation either.
        assert!(!ChError::from_server(clickhouse::error::Error::TimedOut, None).is_cancelled());
    }

    /// ClickHouse counts the offset in bytes and the trait counts it in
    /// characters, and the two agree on every statement anybody writes in
    /// English.
    ///
    /// Both statements and both numbers are what ClickHouse 24.10 actually
    /// answered; the assertion is that the caret lands on the `B` of `BY` in
    /// each, which is only true for one of the two ways of counting.
    #[test]
    fn a_position_is_counted_in_characters_and_not_bytes() {
        let ascii = "SELECT id FROM bench.bench_wide WHERE ORDER BY id";
        let exception = "Code: 62. DB::Exception: Syntax error: failed at position 45 ('BY')";
        assert_eq!(position(exception, ascii), Some(45));
        assert_eq!(ascii.chars().nth(44), Some('B'));

        // The same statement with a CJK identifier in it. The server says 63
        // for a character that is the 51st, because six of the characters
        // before it are three bytes each.
        let unicode = "SELECT \"漢字漢字漢字\" FROM bench.bench_wide WHERE ORDER BY id";
        let exception = "Code: 62. DB::Exception: Syntax error: failed at position 63 ('BY')";
        assert_eq!(position(exception, unicode), Some(51));
        assert_eq!(unicode.chars().nth(50), Some('B'));
    }

    /// An offset past the end is one that belongs to some other text — a
    /// `DESCRIBE (…)` wrapper, say — and a caret placed from it would land
    /// wherever the string happened to reach.
    #[test]
    fn an_offset_that_cannot_be_this_statement_is_not_reported() {
        assert_eq!(position("failed at position 900 ('x')", "SELECT 1"), None);
        assert_eq!(position("failed at position 0 ('x')", "SELECT 1"), None);
        assert_eq!(position("no offset in here at all", "SELECT 1"), None);
    }
}
