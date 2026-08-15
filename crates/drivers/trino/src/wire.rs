//! The Trino client protocol, which is three HTTP requests and one JSON shape.
//!
//! `POST /v1/statement` with the statement as the body answers with a query id
//! and a `nextUri`. Each `GET` of a `nextUri` answers with the same shape again:
//! maybe `columns`, maybe `data`, maybe another `nextUri`, maybe an `error`. The
//! result is finished when there is no `nextUri`. `DELETE` stops it.
//!
//! Two headers are not optional and one of them is not obvious:
//!
//! - **`X-Trino-User`.** Without it the coordinator answers `401 Basic
//!   authentication or X-Trino-Original-User or X-Trino-User must be sent`,
//!   whether or not it has authentication configured.
//! - **`X-Trino-Client-Capabilities: PARAMETRIC_DATETIME`.** Without it the
//!   server silently rewrites every `timestamp(p)` and `time(p)` to precision 3
//!   *and drops the precision from the type it reports* — `timestamp(9)` comes
//!   back as `timestamp` holding `2024-01-15 12:34:56.123`, three digits where
//!   the table has nine. It is a compatibility default for clients written
//!   before Trino had parametric datetimes, and a client that does not opt in
//!   has no way to tell a truncated value from a stored one. Measured against
//!   Trino 483: the same statement with the header answers `timestamp(9)` and
//!   `2024-01-15 12:34:56.123456789`.
//!
//! **No retry.** The client protocol asks that 429, 502, 503 and 504 be retried
//! with a backoff, and this does not: the retry would happen inside a call the
//! Cancel button cannot reach, so a coordinator restarting would show up as a
//! client that has hung rather than as one that failed. The status reaches the
//! caller instead. That is a gap and not a decision to be proud of; it is stated
//! here rather than discovered.
//!
//! **`nextUri` needs no rewriting**, which is worth saying because the Cassandra
//! driver's cluster metadata did. Trino builds it from the request's own `Host`
//! header, so a coordinator published on a mapped port hands back the address
//! the client dialled rather than the one the node believes it has.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;

use crate::TrinoError;

/// What the coordinator answers, for every request in the chain.
///
/// One shape for the POST and for every GET after it, which is the protocol's
/// best property: there is no separate "first response" to special-case.
#[derive(Debug, Deserialize)]
pub(crate) struct Answer {
    /// The query id, `20260815_044544_00002_rx3n2`, which is what `DELETE`
    /// names and therefore what a canceller has to hold.
    pub id: String,
    #[serde(rename = "nextUri")]
    pub next_uri: Option<String>,
    /// Present from the moment the columns are settled, and `[]` for a statement
    /// with no result set — which is how those two are told apart, since an
    /// absent `columns` only means "not yet".
    pub columns: Option<Vec<Column>>,
    pub data: Option<Vec<Vec<serde_json::Value>>>,
    /// `INSERT`, `CREATE TABLE`, `SET SESSION` — present for anything that is
    /// not a query, and absent for anything that is.
    #[serde(rename = "updateType")]
    pub update_type: Option<String>,
    /// Rows a write changed, where the statement has one. DDL carries an
    /// `updateType` and no count.
    #[serde(rename = "updateCount")]
    pub update_count: Option<u64>,
    pub error: Option<Failure>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Column {
    pub name: String,
    /// The type taken apart. The answer also carries a `type` field holding the
    /// same thing as a display string — `row(n integer, w varchar)` — and
    /// nothing reads it: the mapping needs the parts, and the *declared* type a
    /// structure pane shows comes from `information_schema.columns`, which is
    /// about the table rather than about one result.
    #[serde(rename = "typeSignature")]
    pub signature: TypeSignature,
}

/// A type taken apart, which is the form worth reading.
///
/// `type` is a display string and parsing it back means writing a parser for
/// `map(varchar(1), array(row(a integer)))`. `typeSignature` is the same type
/// already parsed by the server, so the mapping matches on `raw_type` and reads
/// the precision out of `arguments` instead.
#[derive(Debug, Deserialize)]
pub(crate) struct TypeSignature {
    #[serde(rename = "rawType")]
    pub raw_type: String,
    #[serde(default)]
    pub arguments: Vec<TypeArgument>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TypeArgument {
    /// `LONG` for a precision or a length, `TYPE` for an element type,
    /// `NAMED_TYPE` for a row field.
    pub kind: String,
    pub value: serde_json::Value,
}

impl TypeSignature {
    /// The first numeric argument — a precision, a scale or a length.
    pub fn number(&self, at: usize) -> Option<i64> {
        let argument = self.arguments.get(at)?;
        (argument.kind == "LONG").then(|| argument.value.as_i64())?
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Failure {
    pub message: String,
    /// Trino's own numeric code. Keyed on rather than `errorName`, for the
    /// reason the ClickHouse driver gives: the number is the identifier and the
    /// name is prose.
    #[serde(rename = "errorCode")]
    pub code: Option<i32>,
    /// Where in the statement the fault is, when the fault is in the statement.
    #[serde(rename = "errorLocation")]
    pub location: Option<Location>,
}

/// A 1-based line and a 1-based column, both counted in **code points**.
///
/// Not bytes, and not UTF-16 code units, which is the surprise and the reason
/// `crate::position` has no arithmetic in it beyond counting lines. Measured
/// against Trino 483 with three statements that put the same fault at the same
/// place: ASCII, six CJK characters ahead of it, and seven characters outside
/// the basic plane ahead of it. The three answers differ by exactly the number
/// of characters, which is only true of one of the three ways of counting.
#[derive(Debug, Deserialize)]
pub(crate) struct Location {
    #[serde(rename = "lineNumber")]
    pub line: u32,
    #[serde(rename = "columnNumber")]
    pub column: u32,
}

/// One HTTP client aimed at one coordinator.
///
/// Cheap to clone — `hyper_util`'s client is a pool behind an `Arc` — which is
/// why a canceller can carry one and reach the coordinator while a fetch has the
/// reader borrowed. The situation the PostgreSQL driver needs a second
/// connection for does not arise over HTTP.
#[derive(Clone)]
pub(crate) struct Wire {
    client: Client<HttpConnector, Full<Bytes>>,
    /// `http://host:port`, with no trailing slash.
    origin: String,
    user: String,
    /// The catalog and schema unqualified names resolve in, sent as
    /// `X-Trino-Catalog` and `X-Trino-Schema`. Empty where the connection string
    /// named none, in which case every statement has to qualify its own tables —
    /// which is what the server says: *Catalog must be specified when session
    /// catalog is not set*.
    catalog: String,
    schema: String,
}

impl Wire {
    pub fn new(origin: String, user: String, catalog: String, schema: String) -> Self {
        Self {
            client: Client::builder(TokioExecutor::new()).build_http(),
            origin,
            user,
            catalog,
            schema,
        }
    }

    pub fn catalog(&self) -> &str {
        &self.catalog
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Starts `sql` and returns the first answer, which carries the query id.
    pub async fn post(&self, sql: &str) -> Result<Answer, TrinoError> {
        let uri = format!("{}/v1/statement", self.origin);
        self.send(Method::POST, &uri, Full::new(Bytes::from(sql.to_string())))
            .await
    }

    /// Reads the next answer in the chain.
    pub async fn advance(&self, uri: &str) -> Result<Answer, TrinoError> {
        self.send(Method::GET, uri, Full::default()).await
    }

    /// Asks the coordinator to abandon a query, by id.
    ///
    /// The client protocol's own cancel is a `DELETE` of the current `nextUri`,
    /// and this is not that. The `nextUri` moves with every page, so a canceller
    /// holding one would be holding an address the reader has already left; the
    /// query id is chosen by the coordinator once and never changes. Both were
    /// measured against Trino 483 and produce the same outcome — the query ends
    /// `FAILED` with `USER_CANCELED`, and the reader's next answer carries that
    /// error — including for a `GET` that was already parked on the socket, which
    /// returned within milliseconds of the `DELETE` landing.
    ///
    /// Naming a query that has finished, or one that was never there, answers
    /// `204` rather than `404`. That is what makes cancelling an idle cursor a
    /// no-op instead of an error.
    pub async fn cancel(&self, query_id: &str) -> Result<(), TrinoError> {
        let uri = format!("{}/v1/query/{query_id}", self.origin);
        let request = self
            .headers(Request::builder().method(Method::DELETE).uri(&uri))
            .body(Full::<Bytes>::default())
            .map_err(|e| TrinoError::Transport(format!("{uri}: {e}")))?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| TrinoError::Transport(format!("{uri}: {e}")))?;
        if !response.status().is_success() {
            return Err(TrinoError::Transport(format!(
                "cancelling {query_id}: the coordinator answered {}",
                response.status()
            )));
        }
        Ok(())
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        body: Full<Bytes>,
    ) -> Result<Answer, TrinoError> {
        let request = self
            .headers(Request::builder().method(method).uri(uri))
            .body(body)
            .map_err(|e| TrinoError::Transport(format!("{uri}: {e}")))?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| TrinoError::Transport(format!("{uri}: {e}")))?;

        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| TrinoError::Transport(format!("{uri}: reading the answer: {e}")))?
            .to_bytes();
        if status != StatusCode::OK {
            // The body rather than the status alone: a 401 says which header is
            // missing and a 400 says what it disliked, and neither is guessable
            // from the number.
            return Err(TrinoError::Transport(format!(
                "the coordinator answered {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            TrinoError::Transport(format!("the coordinator's answer did not parse: {e}"))
        })
    }

    /// The headers every request carries, in one place because leaving one off
    /// a single request is how a session stops being one.
    fn headers(&self, builder: hyper::http::request::Builder) -> hyper::http::request::Builder {
        let mut builder = builder
            .header("X-Trino-User", &self.user)
            // What the coordinator's UI and query log show this connection as.
            // Free, and the difference between a slow query somebody can trace
            // and one attributed to nothing.
            .header("X-Trino-Source", "dbclient")
            .header("X-Trino-Client-Capabilities", "PARAMETRIC_DATETIME");
        if !self.catalog.is_empty() {
            builder = builder.header("X-Trino-Catalog", &self.catalog);
        }
        if !self.schema.is_empty() {
            builder = builder.header("X-Trino-Schema", &self.schema);
        }
        builder
    }
}
