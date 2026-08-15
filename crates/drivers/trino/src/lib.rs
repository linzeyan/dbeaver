//! Trino, read over its HTTP client protocol as Arrow.
//!
//! The first database here that is not a database. Trino stores nothing: a
//! catalog is a connector pointed at somewhere else, and one statement can join
//! a Hive table to a PostgreSQL one. Most of the trait fits anyway, which is the
//! useful result. Five places did not, and they are the finding.
//!
//! **The namespace has three levels and the trait has two.** `catalog.schema
//! .table`, where the catalog is which system the data is in. A schema here is
//! therefore called `catalog.schema` — `tpch.tiny`, `memory.default` — which is
//! DuckDB's answer to the same problem and is taken deliberately rather than
//! reinvented. `browse` splits it back apart and quotes each half, which is one
//! step further than DuckDB goes and is worth the difference: DuckDB pastes the
//! composite in unquoted, and a Trino catalog reached through a connector called
//! `Sales` would then be looked up as `sales`, which is a different catalog or
//! none. The split is at the *first* dot, because a catalog is named by a
//! properties file on the coordinator and a schema is named by whatever system
//! is behind it — so the schema is the half that can contain a dot.
//!
//! **A cursor and a query are the same call**, as in ClickHouse and for the same
//! reason, arrived at from a different direction. Trino's protocol is a chain:
//! every answer carries the URI of the next one, the chain runs forward only,
//! and it is one execution of one statement. That is exactly the pair of
//! properties `Driver::cursor` asks for — page *n* costs what page one costs, and
//! the pages agree with each other because there is only ever one plan producing
//! them. `LIMIT`/`OFFSET`, which the trait exists instead of, is not reached for.
//!
//! **The page size is the caller's and never the server's.** Trino chunks by
//! bytes: `SELECT * FROM tpch.tiny.lineitem` arrived in nine chunks of 6851 to
//! 9440 rows, and `SELECT orderkey FROM tpch.tiny.orders` arrived in exactly one
//! of 15000. So the carry has to work in both directions — accumulate chunks
//! that are too small, split chunks that are too large — where the ClickHouse and
//! Cassandra drivers only ever had to accumulate.
//!
//! **Nothing about a session survives between statements, and this driver does
//! not make it.** Every statement is a `POST` that stands alone; `SET SESSION`,
//! `USE` and `START TRANSACTION` all answer with a response header the client is
//! expected to send back on everything afterwards, and this driver drops them.
//! That is ClickHouse's arrangement and it is chosen for ClickHouse's reason —
//! statelessness is what makes a cancel an ordinary second request instead of a
//! contended one — but the cost here is larger and is stated rather than hidden:
//! a `SET SESSION` typed into the editor succeeds and changes nothing that
//! follows it. `transactional` is the visible consequence and `driver.rs` argues
//! it at length.
//!
//! **Five metadata calls are structurally empty**, and for once that is not a
//! judgement call. Trino's `information_schema` has eight tables and none of them
//! is `table_constraints`, `key_column_usage`, `referential_constraints`,
//! `check_constraints`, `statistics` or `triggers`; asking for any of the six is
//! `TABLE_NOT_FOUND`. And the grammar agrees: `CREATE INDEX` and `CREATE TRIGGER`
//! are syntax errors whose message lists what `CREATE` does accept — *BRANCH,
//! CATALOG, FUNCTION, MATERIALIZED, OR, ROLE, SCHEMA, TABLE, VIEW* — and
//! `PRIMARY KEY` inside a `CREATE TABLE` is a syntax error too. See `metadata.rs`.
//!
//! One thing came out better than expected. Trino reports a fault as `line 1:45`
//! and the column is counted in **code points**, not bytes and not UTF-16 code
//! units — so `position` below has no byte arithmetic in it and no clamp for the
//! supplementary plane, which is one better than ClickHouse and one better than
//! Cassandra respectively.

mod arrow_map;
mod driver;
mod metadata;
mod wire;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow_map::Plan;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use wire::{Answer, Failure, Location, Wire};

/// Trino's port when a connection string names none.
const DEFAULT_PORT: u16 = 8080;

/// The user a connection string that names none is given.
///
/// Trino refuses a request with no user at all — `401 Basic authentication or
/// X-Trino-Original-User or X-Trino-User must be sent` — even on a coordinator
/// with no authentication configured, so there has to be one. The client's own
/// name rather than the operating system's, which is what the Trino CLI uses:
/// the OS user is right for a terminal and wrong for a desktop application,
/// where it would attribute every query to whoever is logged into the laptop.
const DEFAULT_USER: &str = "dbclient";

/// Trino's own code for a query somebody stopped.
///
/// `USER_CANCELED`, and 3 rather than the name for the reason the ClickHouse
/// driver gives: the number is the identifier and the name is prose. It has to
/// be told apart from `EXCEEDED_TIME_LIMIT`, which is a different thing arriving
/// through the same field and is not a button anybody pressed.
const USER_CANCELED: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum TrinoError {
    /// A statement the coordinator refused, with the two facts a front end acts
    /// on already read out of the answer before the rest became a string.
    #[error("{message}")]
    Query {
        message: String,
        /// Trino's own error code, where the failure reached the coordinator at
        /// all.
        code: Option<i32>,
        /// 1-based, counted in characters, into the text the caller wrote.
        position: Option<u32>,
    },
    /// A request that did not get an answer, or got one that was not a Trino
    /// answer: no route, nothing listening, a 401, an HTML error page from a
    /// proxy in the way.
    #[error("{0}")]
    Transport(String),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("{0}")]
    BadUrl(String),
}

impl TrinoError {
    /// Reads a coordinator failure for the facts a front end needs, resolving
    /// any statement offset against `sent`.
    ///
    /// `sent` must be the text the caller wrote, or `None`. The metadata queries
    /// pass `None`, because an offset into `SELECT … FROM tpch
    /// .information_schema.columns` would put a caret in text the user never
    /// saw. Nothing in this driver rewrites a user's statement, so wherever a
    /// user's statement is what failed the two are the same string.
    fn from_server(failure: &Failure, sent: Option<&str>) -> Self {
        TrinoError::Query {
            message: failure.message.clone(),
            code: failure.code,
            position: sent
                .zip(failure.location.as_ref())
                .and_then(|(sql, at)| position(at, sql)),
        }
    }

    /// Whether the coordinator stopped this statement because somebody asked it
    /// to.
    ///
    /// Read from the code the coordinator sent rather than from this side
    /// remembering that it pressed Cancel: a statement can fail on its own
    /// merits in the same moment the `DELETE` lands, and reporting that as
    /// cancelled hides a real fault behind a button.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, TrinoError::Query { code, .. } if *code == Some(USER_CANCELED))
    }

    /// Where in the statement the coordinator says the trouble is: 1-based,
    /// counted in characters.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            TrinoError::Query { position, .. } => *position,
            _ => None,
        }
    }
}

/// A Trino error location as the offset the trait asks for.
///
/// Trino says `line 1:45`, a 1-based line and a **1-based column counted in code
/// points**. That last part is the whole of why this function is short: there is
/// no byte arithmetic, as ClickHouse needs, and no clamp for characters outside
/// the basic plane, as Cassandra needs. Measured against Trino 483 with the same
/// fault reached three ways — ASCII, six CJK characters ahead of it, seven
/// supplementary-plane characters ahead of it — and the three answers differed by
/// exactly the character count each time.
///
/// The clamp that is here is for a column past the end of its line, which is
/// what a fault reported at end-of-input looks like. One past the end is a
/// position the trait allows; two past it is a caret outside the statement.
fn position(at: &Location, sql: &str) -> Option<u32> {
    if at.line == 0 || at.column == 0 {
        return None;
    }
    let mut before = 0usize;
    for (index, line) in sql.split('\n').enumerate() {
        let width = line.chars().count();
        if index as u32 + 1 == at.line {
            return u32::try_from(before + (at.column as usize).min(width + 1)).ok();
        }
        // The newline itself, which is one character in the text the caller
        // wrote and no character on either line.
        before += width + 1;
    }
    None
}

/// A name as Trino spells one, for the statements this driver writes itself.
///
/// Always quoted, never conditionally, which is the opposite of what `browse`
/// does and deliberately so. These names go into a catalog query nobody reads,
/// where an unquoted `Sales` folding to `sales` would silently answer for a
/// different catalog; a browse statement is shown to the person about to run it,
/// where quoting everything is correct and unreadable.
pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A value as a SQL string literal.
///
/// Interpolated rather than bound, and that is a real cost worth naming: the
/// client protocol does carry a prepared statement, in an `X-Trino-Prepared
/// -Statement` header, executed by a second statement of the form `EXECUTE name
/// USING …`. Buying escaping that way means every catalog query becoming two
/// headers and a rewrite, to replace one function whose whole content is
/// doubling a quote — which is the same rule the lexer in `dbsql` reads one by.
pub(crate) fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A `catalog.schema` name split back into the two levels Trino has.
///
/// At the first dot and not the last. A catalog is named by a properties file on
/// the coordinator and cannot contain one in any deployment this could reach —
/// `CREATE CATALOG` is the only other way to make one, and it is refused by the
/// static catalog store a stock coordinator runs. A schema is named by whatever
/// system the connector points at, and a Hive schema with a dot in it is
/// somebody's ordinary Tuesday.
///
/// `None` where there is no dot, which is a schema string that never came from
/// `schemas()`. Every caller answers that with an empty result rather than a
/// statement naming a catalog that is not there.
pub(crate) fn parts(schema: &str) -> Option<(&str, &str)> {
    schema.split_once('.')
}

/// The registry of statements this session has in flight, by query id.
///
/// A `std::sync::Mutex` and not tokio's, because a reader removes its own entry
/// from `Drop`, which cannot await.
type Live = Arc<Mutex<HashMap<u64, String>>>;

/// One session against one Trino coordinator.
///
/// There is no connection to hold: the client protocol is stateless HTTP, and
/// `Wire` is a pooled hyper client behind a clone. That is what makes `cancel` an
/// ordinary second request rather than something needing a connection of its own,
/// and it is also why `transactional` is false — see `driver.rs`.
pub struct TrinoSource {
    wire: Arc<Wire>,
    live: Live,
    next: AtomicU64,
}

impl TrinoSource {
    /// Connects to `url`, of the form
    /// `http://user@host:port/catalog/schema`.
    ///
    /// The catalog and the schema are both optional and both become session
    /// defaults, so that a statement typed into the editor can say `SELECT *
    /// FROM orders`. Without them the coordinator answers *Catalog must be
    /// specified when session catalog is not set*, which is a good message and a
    /// poor default.
    ///
    /// The round trip at the end proves two things at once, and the second is
    /// the one worth the request: that the coordinator is there and answering,
    /// and that the catalog named in the URL exists. Trino will not do the
    /// second for free — a `SELECT 1` sent with `X-Trino-Catalog:
    /// no_such_catalog` succeeds, because nothing in it resolves a name — so a
    /// driver that only proved reachability would report success and then fail
    /// on the first table anybody clicked.
    pub async fn connect(url: &str) -> Result<Self, TrinoError> {
        let parsed = url::Url::parse(url).map_err(|e| TrinoError::BadUrl(format!("{url}: {e}")))?;
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| TrinoError::BadUrl(format!("{url}: no host")))?;
        let origin = format!(
            "{}://{host}:{}",
            parsed.scheme(),
            parsed.port().unwrap_or(DEFAULT_PORT)
        );
        let user = match percent_decode(parsed.username()) {
            name if name.is_empty() => DEFAULT_USER.to_string(),
            name => name,
        };
        let mut path = parsed
            .path()
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(percent_decode);
        let catalog = path.next().unwrap_or_default();
        let schema = path.next().unwrap_or_default();

        let source = Self {
            wire: Arc::new(Wire::new(origin, user, catalog.clone(), schema)),
            live: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(0),
        };

        if catalog.is_empty() {
            source.ask("SELECT 1").await?;
        } else {
            let found = source
                .ask(&format!(
                    "SELECT catalog_name FROM system.metadata.catalogs WHERE catalog_name = {}",
                    literal(&catalog)
                ))
                .await?;
            if found.is_empty() {
                return Err(TrinoError::Query {
                    message: format!("this coordinator has no catalog called {catalog}"),
                    code: None,
                    position: None,
                });
            }
        }
        Ok(source)
    }

    /// The catalog unqualified names resolve in, or empty where there is none.
    pub fn catalog(&self) -> &str {
        self.wire.catalog()
    }

    /// The schema unqualified names resolve in, or empty where there is none.
    pub fn schema(&self) -> &str {
        self.wire.schema()
    }

    /// Runs `sql` and streams its result as Arrow batches of `batch_rows` rows.
    ///
    /// Resolves once the columns are known and before any row has been handed
    /// over, so a caller can lay out a grid immediately. That costs the two or
    /// three round trips the statement spends `QUEUED` — the protocol has no
    /// `DESCRIBE`, and the columns arrive in the chain at the moment the plan is
    /// settled. Nothing is wasted: those are the same requests the caller was
    /// going to make anyway, and any rows that arrive with the columns are kept.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<Rows, TrinoError> {
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

    /// Reads `sql` forward, a page at a time.
    ///
    /// The same mechanism as `query`, for the reason in the module comment: the
    /// chain of `nextUri`s already is one execution read forward without
    /// re-reading, and there is no second mechanism to reach for. What differs is
    /// that this one is not registered with the session, so `cancel` does not
    /// reach it — the trait says a session cancel does not touch a cursor, and
    /// this is where that is true rather than remembered.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Rows, TrinoError> {
        self.read(sql, batch_rows, None).await
    }

    async fn read(
        &self,
        sql: &str,
        batch_rows: usize,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, TrinoError> {
        Rows::open(Arc::clone(&self.wire), sql, batch_rows.max(1), register).await
    }

    /// Asks the coordinator to abandon whatever this session is running.
    ///
    /// One `DELETE` per statement in flight, because HTTP is stateless and a
    /// cancel contends with nothing — the situation the PostgreSQL driver needs a
    /// second connection for does not arise.
    ///
    /// Best-effort, as the trait says: success means the request was delivered,
    /// not that anything stopped. A session with nothing running sends nothing at
    /// all, because cancelling an idle session is a no-op and a round trip that
    /// proves it is a round trip wasted.
    pub async fn cancel(&self) -> Result<(), TrinoError> {
        let ids: Vec<String> = match self.live.lock() {
            Ok(live) => live.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        for id in ids {
            self.wire.cancel(&id).await?;
        }
        Ok(())
    }

    /// Runs one catalog query and hands back its rows as they arrived.
    ///
    /// JSON and not Arrow, which is the whole reason this exists beside `query`:
    /// `metadata.rs` wants strings out of eight columns, and building a
    /// `RecordBatch` to read them back out of would be a type mapping in the
    /// path of every navigator click.
    pub(crate) async fn ask(&self, sql: &str) -> Result<Vec<Vec<Value>>, TrinoError> {
        let mut answer = self.wire.post(sql).await?;
        let mut rows = Vec::new();
        loop {
            if let Some(failure) = &answer.error {
                return Err(TrinoError::from_server(failure, None));
            }
            if let Some(data) = answer.data.take() {
                rows.extend(data);
            }
            let Some(uri) = answer.next_uri.clone() else {
                return Ok(rows);
            };
            answer = self.wire.advance(&uri).await?;
        }
    }
}

fn percent_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// Puts a statement's query id where `TrinoSource::cancel` can find it, and takes
/// it back out when the reader is dropped.
///
/// A query id whose statement has finished is not something to leave lying in a
/// list — the `DELETE` would name it, match nothing, and cost a round trip
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
/// Both a `ResultStream` and a `Cursor`, because against Trino they are the same
/// object read the same way.
pub struct Rows {
    wire: Arc<Wire>,
    /// The text the caller wrote, kept so a failure's offset resolves against
    /// what they can see.
    text: String,
    plan: Plan,
    query_id: String,
    /// The next link in the chain, or `None` once the coordinator has stopped
    /// offering one — which is what "finished" means in this protocol.
    next: Option<String>,
    /// Rows read out of a chunk but not yet handed over.
    ///
    /// Trino chunks by bytes rather than rows, so a chunk is never the size the
    /// caller asked for: measured at 6851 to 9440 rows for one statement and
    /// 15000 in a single chunk for another. The carry is what makes the page size
    /// the caller's number rather than the coordinator's, and unlike the
    /// ClickHouse and Cassandra carries it has to split as often as it joins.
    carry: Option<RecordBatch>,
    batch_rows: usize,
    delivered: u64,
    /// Set for anything that is not a query: `INSERT`, `CREATE TABLE`, `SET
    /// SESSION`. Its presence is what tells a write apart from a read.
    update_type: Option<String>,
    update_count: Option<u64>,
    _registration: Option<Registration>,
}

impl Rows {
    async fn open(
        wire: Arc<Wire>,
        sql: &str,
        batch_rows: usize,
        register: Option<(Live, u64)>,
    ) -> Result<Rows, TrinoError> {
        let mut answer = wire.post(sql).await?;
        let query_id = answer.id.clone();
        // Held from here rather than from the first successful page, so that a
        // statement cancelled while it is still `QUEUED` is one `cancel` can
        // name.
        let registration =
            register.map(|(live, id)| Registration::hold(live, id, query_id.clone()));

        let mut rows = Rows {
            wire,
            text: sql.to_string(),
            plan: Plan::empty(),
            query_id,
            next: None,
            carry: None,
            batch_rows,
            delivered: 0,
            update_type: None,
            update_count: None,
            _registration: registration,
        };

        // Forward until the columns are settled, which is where `query`'s
        // promise is kept. An answer with no `columns` key at all means the plan
        // is not ready yet; `columns: []` means it is ready and there is no
        // result set, which is a statement that writes.
        loop {
            if let Some(failure) = &answer.error {
                return Err(TrinoError::from_server(failure, Some(sql)));
            }
            rows.record(&mut answer);
            if let Some(columns) = &answer.columns {
                rows.plan = Plan::of(columns);
                rows.next = answer.next_uri.take();
                if let Some(data) = answer.data.take().filter(|d| !d.is_empty()) {
                    rows.carry = Some(rows.plan.batch(&data)?);
                }
                return Ok(rows);
            }
            let Some(uri) = answer.next_uri.take() else {
                // The chain ended before any columns did. Not reachable against a
                // coordinator that is working, and answering with an empty result
                // is the honest shape for it.
                return Ok(rows);
            };
            answer = rows.wire.advance(&uri).await?;
        }
    }

    pub fn schema(&self) -> SchemaRef {
        self.plan.schema()
    }

    /// Rows this statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Three answers, because Trino gives three. A read reports what it produced,
    /// counted here. A write reports what it changed, which the coordinator sends
    /// as `updateCount` and which is a better number than the ClickHouse driver
    /// can offer — an `INSERT` of three rows says three. And a DDL statement
    /// carries an `updateType` with no count at all, so it answers `None`: it did
    /// something, and how much is not a number Trino has.
    ///
    /// `None` and not `0` for that last case, which is the same ambiguity the
    /// ClickHouse driver names — the trait uses `None` for "not read to the end",
    /// and here it also has to mean "never knowable". A wrong number is worse
    /// than a missing one.
    pub fn rows_affected(&self) -> Option<u64> {
        if self.next.is_some() || self.carry.is_some() {
            return None;
        }
        match self.update_type {
            Some(_) => self.update_count,
            None => Some(self.delivered),
        }
    }

    /// The next page, or `None` once the result is fully consumed.
    pub async fn next_page(&mut self) -> Result<Option<RecordBatch>, TrinoError> {
        loop {
            let held = self.carry.as_ref().map_or(0, RecordBatch::num_rows);
            if held >= self.batch_rows {
                return Ok(Some(self.take(self.batch_rows)));
            }
            let Some(uri) = self.next.clone() else {
                return Ok((held > 0).then(|| self.take(held)));
            };

            let mut answer = self.wire.advance(&uri).await?;
            self.next = answer.next_uri.take();
            if let Some(failure) = &answer.error {
                return Err(TrinoError::from_server(failure, Some(&self.text)));
            }
            self.record(&mut answer);
            let Some(data) = answer.data.take().filter(|d| !d.is_empty()) else {
                // An answer with no rows in it is ordinary: the coordinator
                // blocks for up to a second waiting for the query to produce
                // something and then answers anyway, so that a client which has
                // gone away is noticed.
                continue;
            };
            let batch = self.plan.batch(&data)?;
            self.carry = match self.carry.take() {
                None => Some(batch),
                Some(held) => Some(arrow::compute::concat_batches(
                    &self.plan.schema(),
                    &[held, batch],
                )?),
            };
        }
    }

    /// What kind of statement this is, from whichever answer says so.
    ///
    /// Read from every answer rather than only from the one that carried the
    /// columns, because the two do not arrive together: an `INSERT` states its
    /// `updateType` with the columns and its `updateCount` several answers later,
    /// once the write has actually happened.
    fn record(&mut self, answer: &mut Answer) {
        if answer.update_type.is_some() {
            self.update_type = answer.update_type.take();
        }
        if answer.update_count.is_some() {
            self.update_count = answer.update_count;
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

    /// A handle for stopping this reader from another thread.
    ///
    /// Taken out in advance rather than reached for at cancel time, because by
    /// then the reader is borrowed by the fetch that is to be stopped. The query
    /// id it names was chosen by the coordinator before the first page existed
    /// and does not move, which is why it and not the `nextUri` is what a cancel
    /// is addressed to — see `Wire::cancel`.
    pub fn canceller(&self) -> RowsCancel {
        RowsCancel {
            wire: Arc::clone(&self.wire),
            query_id: self.query_id.clone(),
        }
    }

    /// Lets go of the chain and of whatever is held.
    ///
    /// Optional; dropping does the same, which is what keeps the two consistent.
    /// Note what neither of them does: the coordinator is not told. A query
    /// nobody is reading stays alive until `query.client.timeout` — five minutes
    /// on a stock coordinator — and then abandons itself. Stopping it sooner is
    /// what `canceller` is for, and it is a `DELETE` that cannot be sent from a
    /// `Drop`.
    pub async fn close(&mut self) -> Result<(), TrinoError> {
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
    query_id: String,
}

impl RowsCancel {
    /// Delivered is not interrupted. A statement that had already finished
    /// leaves nothing to stop and this still succeeds — the coordinator answers
    /// `204` for a query it has forgotten — and what actually happened shows up
    /// as the reader failing with `is_cancelled`, or not failing at all.
    pub async fn cancel(&self) -> Result<(), TrinoError> {
        self.wire.cancel(&self.query_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no server — it needs the absence of one. Port 1 is reserved and
    /// nothing on a developer machine or a CI runner listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let error = TrinoSource::connect("http://127.0.0.1:1/memory")
            .await
            .err()
            .expect("nothing is listening on port 1");
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains("refused") || message.contains("connect"),
            "the refusal should survive into the message, got: {message}"
        );
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_before_anything_is_sent() {
        let error = TrinoSource::connect("not a url at all")
            .await
            .err()
            .unwrap();
        assert!(matches!(error, TrinoError::BadUrl(_)));
    }

    /// Trino counts the column in code points and so does the trait, which makes
    /// this the second driver here with no byte arithmetic in it — and exactly
    /// the reason to pin it, since a change in the coordinator's counting would
    /// otherwise show up as a caret quietly landing in the wrong place.
    ///
    /// All three statements and all three numbers are what Trino 483 actually
    /// answered; the assertion is that the caret lands on the `O` of `ORDER` in
    /// each, which is true of exactly one of the three ways of counting.
    #[test]
    fn a_position_is_counted_in_code_points_and_not_bytes_or_utf16() {
        let at = |line, column| Location { line, column };

        let ascii = "SELECT orderkey FROM tpch.tiny.orders WHERE ORDER BY orderkey";
        assert_eq!(position(&at(1, 45), ascii), Some(45));
        assert_eq!(ascii.chars().nth(44), Some('O'));

        // Six CJK characters ahead of the fault. Three bytes each, so a byte
        // offset would have been 57.
        let cjk = "SELECT \"漢字漢字漢字\" FROM tpch.tiny.orders WHERE ORDER BY orderkey";
        assert_eq!(position(&at(1, 45), cjk), Some(45));
        assert_eq!(cjk.chars().nth(44), Some('O'));

        // Seven characters outside the basic plane, which is where a UTF-16
        // count would part company: two code units each, so it would have said
        // 53 for the character that is the 46th.
        let astral = "SELECT \"𝔘𝔫𝔦𝔠𝔬𝔡𝔢\" FROM tpch.tiny.orders WHERE ORDER BY orderkey";
        assert_eq!(position(&at(1, 46), astral), Some(46));
        assert_eq!(astral.chars().nth(45), Some('O'));
    }

    /// The line is 1-based and so is the column, which is two chances to be off
    /// by one in a statement long enough for it to matter.
    #[test]
    fn an_offset_on_a_later_line_counts_the_lines_before_it() {
        let sql = "SELECT orderkey FROM tpch.tiny.orders\nWHERE ORDER BY orderkey";
        let at = position(&Location { line: 2, column: 7 }, sql).expect("an offset") as usize;
        assert_eq!(sql.chars().nth(at - 1), Some('O'));
    }

    /// A caret one past the end is a place a front end can put one; two past it
    /// is outside the statement.
    #[test]
    fn an_offset_that_cannot_be_this_statement_is_clamped_or_declined() {
        let sql = "SELECT 1";
        assert_eq!(
            position(&Location { line: 1, column: 9 }, sql),
            Some(9),
            "one past the end is where end-of-input faults point"
        );
        assert_eq!(
            position(
                &Location {
                    line: 1,
                    column: 40
                },
                sql
            ),
            Some(9)
        );
        assert_eq!(position(&Location { line: 9, column: 1 }, sql), None);
        assert_eq!(position(&Location { line: 0, column: 1 }, sql), None);
        assert_eq!(position(&Location { line: 1, column: 0 }, sql), None);
    }

    /// A catalog cannot hold a dot and a schema can, so the split is at the
    /// first one — and a name that never came from `schemas()` splits into
    /// nothing rather than into a guess.
    #[test]
    fn a_composite_schema_splits_at_the_catalog_and_not_at_the_last_dot() {
        assert_eq!(parts("tpch.tiny"), Some(("tpch", "tiny")));
        assert_eq!(parts("hive.year.2024"), Some(("hive", "year.2024")));
        assert_eq!(parts("memory"), None);
        assert_eq!(parts(""), None);
    }

    /// The two ways this driver writes a name into a statement it composes.
    #[test]
    fn a_name_with_a_delimiter_in_it_survives_being_written_down() {
        assert_eq!(quote(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(literal("O'Brien"), "'O''Brien'");
    }
}
