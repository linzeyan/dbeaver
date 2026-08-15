//! Snowflake, read over its SQL API v2 as Arrow.
//!
//! **No Snowflake server has ever answered this driver.** There is no account
//! behind it, no trial, no container and no emulator; every line of it is read
//! off the published SQL API reference and written without a single request
//! having been sent. Every other driver in this workspace earned its place by
//! being run against the real thing — that is what the contract suite in
//! `crates/conn/tests/contract.rs` is, and it is why this driver has no subject
//! there. An absent subject is the honest report; a mocked one would make a
//! suite green and establish nothing.
//!
//! So this is what a reviewer is holding: code that is *consistent with the
//! documentation*, which is a weaker thing than code that works, and the
//! difference is not knowable from here.
//!
//! ## What will only be known when a server answers
//!
//! 1. **Whether the account accepts the token.** `auth.rs` is checked as far as
//!    arithmetic can be — the fingerprint against openssl's, the signature
//!    against RS256 — but "this JWT is well formed" and "Snowflake logs this
//!    user in" are different claims. The account-identifier rule in particular
//!    (upper case, region dropped, `.global` handled separately) fails as
//!    `JWT token is invalid` and nothing more specific.
//! 2. **Whether `SELECT CURRENT_VERSION()` runs without a warehouse.** It is the
//!    round trip `connect` uses to prove the credentials, chosen because context
//!    functions are said to be answered by the cloud services layer. If that is
//!    wrong, a connection to an account with no default warehouse fails at the
//!    dialog rather than at the first statement.
//! 3. **The `jsonv2` encoding of every type.** That all values arrive as JSON
//!    strings, that a `DATE` is a day number, that a `TIME` and a `TIMESTAMP` are
//!    seconds with a fraction, that a `TIMESTAMP_TZ` carries its offset in
//!    minutes plus 1440, that a `BINARY` is hex, and how a `FLOAT` holding NaN is
//!    spelled. `arrow_map.rs` marks each of these where it is decided, and every
//!    one of them fails loudly rather than producing a wrong value.
//! 4. **The code for a statement somebody stopped**, `000604`. It is what tells
//!    a pressed Cancel button apart from a fault, and a wrong code shows up as an
//!    error banner where there should be none.
//! 5. **Whether the four `SHOW` commands in `metadata.rs` name their columns the
//!    way this driver looks them up.** They are read by name rather than by
//!    position, which is the safer of the two and still assumes the names.
//! 6. **The polling schedule.** How long an account takes to turn a `202` into a
//!    `200` was guessed, not measured; see `Rows::settle`.
//! 7. **Whether anything reports a position in a failed statement.** The API
//!    carries no field for one and Snowflake writes `at position 7` into the
//!    prose. This driver reports no position at all rather than parsing a
//!    sentence it has never seen.
//!
//! ## What the protocol is
//!
//! Three levels of namespace — `database.schema.table` — where the trait has
//! two, so a schema here is called `database.schema`. That is the DuckDB and
//! Trino answer to the same problem and is taken deliberately; `driver.rs` says
//! how the two halves are split apart again.
//!
//! A cursor and a query are the same call, for the third distinct reason in this
//! workspace. A Snowflake statement produces a result already divided into
//! partitions, and a partition is fetched by index from a handle that does not
//! move. Page *n* costs what page one costs and nothing is re-read, which is both
//! properties the trait asks a cursor for; `LIMIT`/`OFFSET`, which the trait
//! exists instead of, is not reached for.
//!
//! Nothing about a session survives between statements, exactly as in the Trino
//! and ClickHouse drivers, and here it is the API's own arrangement rather than
//! this driver's choice: there is no session to hold. The database, schema,
//! warehouse and role travel in the body of every request. A `USE WAREHOUSE`
//! typed into the editor therefore succeeds and changes nothing after it, and
//! `transactional` is false — `driver.rs` argues that at length.

mod arrow_map;
mod auth;
mod driver;
mod metadata;
mod wire;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow_map::Plan;
use auth::{Credential, Signer};
use hyper::StatusCode;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wire::{Answer, Session, Wire};

/// The host suffix every Snowflake account is reached at.
///
/// Stripped off to get the account identifier the JWT claims name. A host that
/// does not end with it — somebody's proxy, or a form Snowflake adds later — is
/// used whole, and the normalisation in `auth` then takes everything after the
/// first dot off anyway.
const HOST_SUFFIX: &str = ".snowflakecomputing.com";

/// Snowflake's own code for a statement somebody stopped.
///
/// A string and not a number, because it arrives as `"000604"` and parsing that
/// to 604 means remembering to put the zeros back. It has to be told apart from
/// a statement that failed on its own merits in the same moment the cancel
/// landed — reporting that as cancelled would hide a real fault behind a button.
const USER_CANCELED: &str = "000604";

/// How long to wait before asking a running statement whether it is done, and
/// the ceiling that wait doubles up to.
///
/// Guessed rather than measured, and the guess is stated here rather than buried
/// in the loop. The shape is what matters: a statement that finishes in a
/// hundred milliseconds should not wait a second to be noticed, and one that
/// runs for ten minutes should not be asked about six hundred times.
const FIRST_POLL: Duration = Duration::from_millis(100);
const LONGEST_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum SnowflakeError {
    /// A statement the account refused, with its own code kept beside the
    /// message — that is what tells a cancel apart from a fault.
    #[error("{message}")]
    Query {
        message: String,
        code: Option<String>,
    },
    /// A request that did not get an answer, or got one that was not the API's:
    /// no route, a TLS failure, a proxy's HTML error page.
    #[error("{0}")]
    Transport(String),
    /// Something wrong with the credentials before anything was sent — an
    /// unreadable key, a connection string naming none.
    #[error("{0}")]
    Auth(String),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl SnowflakeError {
    /// Whether the account stopped this statement because somebody asked it to.
    ///
    /// Read from the code the account sent rather than from this side
    /// remembering that it pressed Cancel, for the reason the Trino driver
    /// gives: a statement can fail on its own merits in the same moment the
    /// cancel lands.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, SnowflakeError::Query { code, .. } if code.as_deref() == Some(USER_CANCELED))
    }
}

/// An API answer as a failure.
///
/// The message alone is what a person reads — `SQL compilation error:\nObject
/// 'ORDERS' does not exist` — and the code is kept because `is_cancelled` is the
/// one thing the message cannot be asked.
fn failure(answer: &Answer) -> SnowflakeError {
    SnowflakeError::Query {
        message: answer
            .message
            .clone()
            .unwrap_or_else(|| "the account refused this statement and said nothing".to_string()),
        code: answer.code.clone(),
    }
}

/// A name as Snowflake spells one, for the statements this driver writes itself.
///
/// **Always quoted, never conditionally**, and that is the opposite of what
/// every other SQL driver here does. An unquoted identifier in Snowflake folds
/// to *upper* case, where PostgreSQL and Trino fold down — so `dbsql`'s rule,
/// "bare when it is already lower case", is precisely backwards here: it would
/// write a column the catalog calls `orders` as a bare `orders`, which resolves
/// to `ORDERS`, which is a different relation or none. Quoting everything is
/// correct for both `ORDERS` and `orders`, and the cost is a statement that
/// reads less nicely.
pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A value as a SQL string literal.
///
/// Interpolated rather than bound, as in the Trino driver and for the same
/// trade: the SQL API does carry bindings, in a `bindings` object of typed
/// values keyed by position, and buying escaping that way means every catalog
/// query below becoming a second structure to build — to replace one function
/// whose whole content is doubling a quote.
pub(crate) fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A `database.schema` name split back into the two levels Snowflake has.
///
/// At the first dot, for the reason the Trino driver gives about catalogs: a
/// database is named by whoever created it and a schema by whoever created it
/// inside that, and neither can hold a dot without quoting — but if one of them
/// does, the schema is the half that is reached last and therefore the half that
/// keeps it.
///
/// `None` where there is no dot, which is a schema string that never came from
/// `schemas()`. Every caller answers that with an empty result rather than a
/// statement naming a database that is not there.
pub(crate) fn parts(schema: &str) -> Option<(&str, &str)> {
    schema.split_once('.')
}

/// The statements this session has in flight, by handle.
///
/// A `std::sync::Mutex` and not tokio's, because a reader removes its own entry
/// from `Drop`, which cannot await.
type Live = Arc<Mutex<HashMap<u64, String>>>;

/// One session against one Snowflake account.
///
/// There is no connection to hold: the SQL API is stateless HTTPS, and `Wire` is
/// a pooled hyper client. That is what makes `cancel` an ordinary second request
/// rather than something needing a connection of its own — the situation the
/// PostgreSQL driver opens a second socket for does not arise — and it is also
/// why `transactional` is false.
pub struct SnowflakeSource {
    wire: Arc<Wire>,
    live: Live,
    next: AtomicU64,
}

impl SnowflakeSource {
    /// Connects to `url`, of the form
    /// `https://user@account.snowflakecomputing.com/database/schema?warehouse=WH&role=R&private_key=/path/to/rsa_key.p8`.
    ///
    /// The database and schema are both optional and both become defaults sent
    /// with every statement, so that a statement typed into the editor can say
    /// `SELECT * FROM ORDERS`. So are the warehouse and the role, and leaving the
    /// warehouse off is the one that will be noticed: without it the account
    /// refuses anything that has to compute, with its own message saying so.
    ///
    /// One of two credentials has to be there. `private_key` names a PEM file
    /// and is the key-pair path — `auth.rs` turns it into the JWT under every
    /// request. `token` carries an OAuth access token somebody else obtained;
    /// this driver does not mint one, because every flow that does is a browser
    /// redirect or an identity provider, and the phase's exit condition is that
    /// cloud authentication works *without* embedded browsers.
    ///
    /// The round trip at the end proves the thing a connection dialog is
    /// actually asking: not that the host resolves, but that the account read the
    /// token and agreed with it. `SELECT CURRENT_VERSION()` is chosen for it
    /// because a context function is answered without a running warehouse — see
    /// the crate comment, where that is one of the things no server has
    /// confirmed.
    pub async fn connect(url: &str) -> Result<Self, SnowflakeError> {
        let parsed =
            url::Url::parse(url).map_err(|e| SnowflakeError::BadUrl(format!("{url}: {e}")))?;
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| SnowflakeError::BadUrl(format!("{url}: no host")))?;
        let origin = match parsed.port() {
            Some(port) => format!("https://{host}:{port}"),
            None => format!("https://{host}"),
        };
        let account = host.strip_suffix(HOST_SUFFIX).unwrap_or(host).to_string();

        let user = percent_decode(parsed.username());
        let mut path = parsed
            .path()
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(percent_decode);
        let session = Session {
            database: path.next().unwrap_or_default(),
            schema: path.next().unwrap_or_default(),
            ..Session::default()
        };
        let (credential, session) = Self::credential(&parsed, &account, &user, session).await?;

        let source = Self {
            wire: Arc::new(Wire::new(origin, credential, session)),
            live: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
        };
        source.ask("SELECT CURRENT_VERSION()").await?;
        Ok(source)
    }

    /// What this connection string proves itself with, and the rest of the
    /// session it names.
    async fn credential(
        parsed: &url::Url,
        account: &str,
        user: &str,
        mut session: Session,
    ) -> Result<(Credential, Session), SnowflakeError> {
        let mut key_file = None;
        let mut token = None;
        for (name, value) in parsed.query_pairs() {
            match name.as_ref() {
                "warehouse" => session.warehouse = value.into_owned(),
                "role" => session.role = value.into_owned(),
                "private_key" => key_file = Some(value.into_owned()),
                "token" => token = Some(value.into_owned()),
                // Anything else is left alone rather than refused. The API takes
                // session parameters this driver has no opinion about, and a
                // connection string that names one should not be rejected by the
                // client that would have passed it on.
                _ => {}
            }
        }

        let credential = match (key_file, token) {
            (Some(path), _) => {
                if user.is_empty() {
                    return Err(SnowflakeError::Auth(
                        "key-pair authentication needs the user the key belongs to, \
                         as in snowflake://user@account.snowflakecomputing.com/…"
                            .to_string(),
                    ));
                }
                let pem = tokio::fs::read_to_string(&path).await.map_err(|e| {
                    SnowflakeError::Auth(format!("this private key could not be read: {path}: {e}"))
                })?;
                Credential::KeyPair(Box::new(Signer::new(&pem, account, user)?))
            }
            (None, Some(token)) => Credential::OAuth(token),
            (None, None) => {
                return Err(SnowflakeError::Auth(
                    "this connection names no credentials. Add private_key=/path/to/rsa_key.p8 \
                     for key-pair authentication, or token=… for an OAuth access token"
                        .to_string(),
                ));
            }
        };
        Ok((credential, session))
    }

    /// The database unqualified names resolve in, or empty where there is none.
    pub fn database(&self) -> &str {
        &self.wire.session().database
    }

    /// The schema unqualified names resolve in, or empty where there is none.
    pub fn schema_name(&self) -> &str {
        &self.wire.session().schema
    }

    /// Runs `sql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the statement has finished and its first partition has
    /// arrived, which is later than the Trino driver's promise and is the API's
    /// doing rather than a choice: there is no answer describing the columns
    /// before the result exists. A caller can still lay out a grid before reading
    /// a row, because the columns are known here.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<Rows, SnowflakeError> {
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

    /// Reads `sql` forward, a partition at a time.
    ///
    /// The same mechanism as `query`, for the reason in the crate comment: a
    /// finished result is already divided into partitions addressed by index, so
    /// there is no second mechanism to reach for. What differs is that this one
    /// is not registered with the session, so `cancel` does not reach it — the
    /// trait says a session cancel does not touch a cursor, and this is where
    /// that is true rather than remembered.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Rows, SnowflakeError> {
        self.read(sql, batch_rows, None).await
    }

    async fn read(
        &self,
        sql: &str,
        batch_rows: usize,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, SnowflakeError> {
        Rows::open(Arc::clone(&self.wire), sql, batch_rows.max(1), register).await
    }

    /// Asks the account to abandon whatever this session is running.
    ///
    /// One cancel per statement in flight, because HTTPS is stateless and a
    /// cancel contends with nothing. Best-effort, as the trait says: success
    /// means the request was delivered, not that anything stopped. A session with
    /// nothing running sends nothing at all.
    pub async fn cancel(&self) -> Result<(), SnowflakeError> {
        let handles: Vec<String> = match self.live.lock() {
            Ok(live) => live.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        for handle in handles {
            self.wire.cancel(&handle).await?;
        }
        Ok(())
    }

    /// Runs one catalog statement and hands back its rows as they arrived.
    ///
    /// JSON and not Arrow, which is why this exists beside `query`: `metadata.rs`
    /// wants strings out of a handful of columns, and building a `RecordBatch` to
    /// read them back out of would put a type mapping in the path of every
    /// navigator click.
    ///
    /// Every partition, because a `SHOW COLUMNS` on a wide schema is not
    /// necessarily one — and a metadata answer that silently stopped at the first
    /// partition would be a navigator that shows some of the tables.
    pub(crate) async fn ask(&self, sql: &str) -> Result<Catalog, SnowflakeError> {
        let mut rows = Rows::open(Arc::clone(&self.wire), sql, usize::MAX, None).await?;
        let names = rows.plan.names();
        let mut data = std::mem::take(&mut rows.pending);
        while rows.next_partition < rows.partitions {
            data.extend(rows.fetch_partition().await?);
        }
        Ok(Catalog { names, rows: data })
    }
}

/// One catalog answer: the rows, and what the columns were called.
///
/// The names matter because half the statements in `metadata.rs` are `SHOW`
/// commands, whose columns are documented by name and whose *order* is not
/// something to depend on — Snowflake has added columns to `SHOW` output before.
/// Looking a column up by name costs a scan of a dozen strings once per call.
pub(crate) struct Catalog {
    names: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Catalog {
    /// The rows, and a way to reach a column of each by name.
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
    /// visible in the navigator, where a failed refresh is not.
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

/// Puts a statement's handle where `SnowflakeSource::cancel` can find it, and
/// takes it back out when the reader is dropped.
struct Registration {
    id: u64,
    live: Live,
}

impl Registration {
    fn hold(live: Live, id: u64, handle: String) -> Self {
        if let Ok(mut held) = live.lock() {
            held.insert(id, handle);
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
/// Both a `ResultStream` and a `Cursor`, because against the SQL API they are the
/// same object read the same way.
pub struct Rows {
    wire: Arc<Wire>,
    handle: String,
    plan: Plan,
    /// How many partitions the whole result has. Zero for a statement with no
    /// result set at all.
    partitions: usize,
    /// The next partition to fetch. One after `open`, because partition zero
    /// arrives with the answer that says the statement finished.
    next_partition: usize,
    /// Rows read but not yet handed over, before they become a batch.
    ///
    /// Used by `ask` only, which wants the JSON rather than Arrow. A statement
    /// read through `query` turns each partition into a batch immediately and
    /// leaves this empty.
    pending: Vec<Vec<Value>>,
    /// Rows built into a batch but not yet handed over. A partition is whatever
    /// size the account chose, so the carry has to split as often as it joins —
    /// the same arrangement the Trino driver needs and for the same reason.
    carry: Option<RecordBatch>,
    batch_rows: usize,
    delivered: u64,
    _registration: Option<Registration>,
}

impl Rows {
    async fn open(
        wire: Arc<Wire>,
        sql: &str,
        batch_rows: usize,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, SnowflakeError> {
        let started = wire.post(sql).await?;
        if !started.status.is_success() && started.status != StatusCode::ACCEPTED {
            return Err(failure(&started.answer));
        }
        let handle = started.answer.handle.clone().ok_or_else(|| {
            SnowflakeError::Transport(
                "the account started a statement without naming it, so nothing here could \
                 read it or stop it"
                    .to_string(),
            )
        })?;
        // Held from here rather than from the first successful page, so that a
        // statement cancelled while it is still queued is one `cancel` can name.
        let registration = register.map(|(live, id)| Registration::hold(live, id, handle.clone()));

        let answer = match started.status {
            StatusCode::OK => started.answer,
            _ => Self::settle(&wire, &handle).await?,
        };

        let metadata = answer.metadata;
        // Checked rather than assumed. `arrow_map` reads a `DATE` as a day
        // number and a `TIMESTAMP` as seconds with a fraction because that is
        // what `jsonv2` is; against any other encoding those become a column of
        // parse failures, and a sentence naming the encoding is a far better
        // thing for somebody to find.
        if let Some(metadata) = &metadata
            && let Some(format) = metadata.format.as_deref()
            && format != "jsonv2"
        {
            return Err(SnowflakeError::Transport(format!(
                "this result is encoded as {format}, and this driver reads jsonv2"
            )));
        }
        let plan = match &metadata {
            Some(metadata) => Plan::of(&metadata.row_type),
            None => Plan::empty(),
        };
        let partitions = metadata.as_ref().map_or(0, |m| m.partitions.len());
        let pending = answer.data.unwrap_or_default();

        Ok(Rows {
            wire,
            handle,
            plan,
            partitions,
            // Partition zero came with the answer above, so the next one to ask
            // for is the first — and a result with no partitions at all is
            // already finished.
            next_partition: partitions.min(1),
            pending,
            carry: None,
            batch_rows,
            delivered: 0,
            _registration: registration,
        })
    }

    /// Waits for a statement to stop running.
    ///
    /// The API does not hold the request open: a `GET` on a statement that is
    /// still going answers `202` straight away, so this is a poll and there is no
    /// version of it that is not. The schedule doubles from a tenth of a second
    /// to two, which keeps a fast statement fast without asking six hundred times
    /// about a slow one. It is a guess — see the crate comment.
    ///
    /// A cancel needs nothing here to notice it: the next `GET` answers with the
    /// account's own cancelled code, and that is what the caller sees.
    async fn settle(wire: &Wire, handle: &str) -> Result<Answer, SnowflakeError> {
        let mut wait = FIRST_POLL;
        loop {
            let reply = wire.poll(handle).await?;
            if reply.status == StatusCode::OK {
                return Ok(reply.answer);
            }
            if reply.status != StatusCode::ACCEPTED {
                return Err(failure(&reply.answer));
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(LONGEST_POLL);
        }
    }

    pub fn schema(&self) -> SchemaRef {
        self.plan.schema()
    }

    /// Rows this statement produced, or `None` until the result has been read to
    /// the end.
    ///
    /// Rows produced, and not rows changed, whatever the statement was — the same
    /// answer the Flight SQL driver gives and for a similar reason. Snowflake
    /// reports a write as a one-row result holding the count, so an `INSERT` of
    /// five rows says 1, being the one row the grid is about to show, and the
    /// number the user wants is in it. Nothing short of parsing the statement
    /// would tell this driver which kind it ran.
    pub fn rows_affected(&self) -> Option<u64> {
        (self.next_partition >= self.partitions && self.carry.is_none() && self.pending.is_empty())
            .then_some(self.delivered)
    }

    /// The next page, or `None` once the result is fully consumed.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, SnowflakeError> {
        if !self.pending.is_empty() {
            let rows = std::mem::take(&mut self.pending);
            self.hold(&rows)?;
        }
        loop {
            let held = self.carry.as_ref().map_or(0, RecordBatch::num_rows);
            if held >= self.batch_rows {
                return Ok(Some(self.take(self.batch_rows)));
            }
            if self.next_partition >= self.partitions {
                return Ok((held > 0).then(|| self.take(held)));
            }
            let rows = self.fetch_partition().await?;
            self.hold(&rows)?;
        }
    }

    /// The next partition's rows, as they arrived.
    async fn fetch_partition(&mut self) -> Result<Vec<Vec<Value>>, SnowflakeError> {
        let reply = self
            .wire
            .partition(&self.handle, self.next_partition as u64)
            .await?;
        if !reply.status.is_success() {
            return Err(failure(&reply.answer));
        }
        self.next_partition += 1;
        Ok(reply.answer.data.unwrap_or_default())
    }

    /// Adds a partition's rows to whatever is already carried.
    fn hold(&mut self, rows: &[Vec<Value>]) -> Result<(), SnowflakeError> {
        if rows.is_empty() {
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
    /// statement handle it names was chosen by the account before the first
    /// partition existed and does not move.
    pub fn canceller(&self) -> RowsCancel {
        RowsCancel {
            wire: Arc::clone(&self.wire),
            handle: self.handle.clone(),
        }
    }

    /// Lets go of whatever is held.
    ///
    /// Optional; dropping does the same. Note what neither of them does: the
    /// account is not told. A finished result stays fetchable by handle for 24
    /// hours whether or not anybody reads it, so there is nothing to release —
    /// stopping a statement that is still *running* is what `canceller` is for.
    pub async fn close(&mut self) -> Result<(), SnowflakeError> {
        self.next_partition = self.partitions;
        self.pending = Vec::new();
        self.carry = None;
        self._registration = None;
        Ok(())
    }
}

/// Stops the statement one reader is running.
#[derive(Clone)]
pub struct RowsCancel {
    wire: Arc<Wire>,
    handle: String,
}

impl RowsCancel {
    /// Delivered is not interrupted. A statement that had already finished
    /// leaves nothing to stop and this still succeeds; what actually happened
    /// shows up as the reader failing with `is_cancelled`, or not failing at all.
    pub async fn cancel(&self) -> Result<(), SnowflakeError> {
        self.wire.cancel(&self.handle).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no server — it needs the absence of one. Port 1 is reserved and
    /// nothing on a developer machine or a CI runner listens there.
    ///
    /// What this pins is that a connection string with credentials in it gets as
    /// far as the network before it fails, rather than being refused for some
    /// nearer reason.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let error =
            SnowflakeSource::connect("https://user@127.0.0.1:1/db/sc?token=ver:1-hint:none")
                .await
                .err()
                .expect("nothing is listening on port 1");
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains("refused") || message.contains("connect"),
            "the refusal should survive into the message, got: {message}"
        );
    }

    /// A connection string with no credentials is refused before a socket is
    /// opened, and the refusal says what to add — this is the most likely thing
    /// to be wrong about a Snowflake connection, and the API's own answer to it
    /// would be a 401 with no advice in it.
    #[tokio::test]
    async fn a_connection_with_no_credentials_says_which_ones_it_wants() {
        let message = SnowflakeSource::connect("https://user@acct.snowflakecomputing.com/db")
            .await
            .err()
            .expect("no credentials")
            .to_string();
        assert!(message.contains("private_key"), "got: {message}");
        assert!(message.contains("token"), "got: {message}");
    }

    /// A key-pair connection with no user cannot build the claims, and saying so
    /// here is better than an account answering `JWT token is invalid`.
    #[tokio::test]
    async fn key_pair_authentication_needs_the_user_the_key_belongs_to() {
        let message =
            SnowflakeSource::connect("https://acct.snowflakecomputing.com/db?private_key=/no/such")
                .await
                .err()
                .expect("no user")
                .to_string();
        assert!(message.contains("user"), "got: {message}");
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_before_anything_is_sent() {
        let error = SnowflakeSource::connect("not a url at all")
            .await
            .err()
            .unwrap();
        assert!(matches!(error, SnowflakeError::BadUrl(_)));
    }

    /// A database cannot be reached without naming it, and a schema string that
    /// never came from `schemas()` splits into nothing rather than into a guess.
    #[test]
    fn a_composite_schema_splits_at_the_database_and_not_at_the_last_dot() {
        assert_eq!(parts("SALES.PUBLIC"), Some(("SALES", "PUBLIC")));
        assert_eq!(parts("SALES.YEAR.2024"), Some(("SALES", "YEAR.2024")));
        assert_eq!(parts("PUBLIC"), None);
        assert_eq!(parts(""), None);
    }

    /// The two ways this driver writes a name into a statement it composes. The
    /// quoting is unconditional, which is the thing to notice: Snowflake folds an
    /// unquoted name *up*, so leaving a lower-case name bare would name a
    /// different relation.
    #[test]
    fn a_name_with_a_delimiter_in_it_survives_being_written_down() {
        assert_eq!(quote(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(quote("orders"), r#""orders""#);
        assert_eq!(quote("ORDERS"), r#""ORDERS""#);
        assert_eq!(literal("O'Brien"), "'O''Brien'");
    }
}
