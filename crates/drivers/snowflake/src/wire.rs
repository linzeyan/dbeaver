//! The Snowflake SQL API, which is four requests and one JSON shape.
//!
//! `POST /api/v2/statements?async=true` answers `202` with a statement handle.
//! `GET /api/v2/statements/<handle>` answers `202` while it is still running and
//! `200` with the first partition once it is not. `GET
//! /api/v2/statements/<handle>?partition=<n>` fetches the rest, one at a time.
//! `POST /api/v2/statements/<handle>/cancel` stops it.
//!
//! **Every request is asynchronous, and that is a choice.** The API will also
//! run a statement inline: a plain `POST` blocks for up to 45 seconds and
//! answers `200` with the result, falling back to `202` only if it runs longer.
//! That is one round trip fewer for a fast statement, and it is not taken,
//! because the handle is the only thing a cancel can name — and with the inline
//! form the handle does not exist on this side until the statement it would stop
//! has already finished or timed out. A Cancel button that works for statements
//! shorter than 45 seconds and not for the ones anybody would press it on is
//! worse than an extra round trip. This is the same position the Trino driver
//! reaches from the other direction, where the query id arrives with the first
//! answer and is registered before any page is read.
//!
//! **No retry**, for the reason the Trino driver states: a retry inside a call
//! the Cancel button cannot reach turns a server having a bad minute into a
//! client that has hung. The status reaches the caller instead.
//!
//! Nothing in this file has been sent to a Snowflake account. The shapes below
//! are transcribed from the published SQL API reference; where the reference is
//! ambiguous the choice is recorded at the field.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde_json::json;

use crate::SnowflakeError;
use crate::auth::Credential;

/// What the account resolves unqualified names in, sent with every statement.
///
/// All four are optional and all four are session state that the SQL API has no
/// other way to carry: there is no session, so a `USE WAREHOUSE` typed into the
/// editor changes nothing that follows it. They are sent on every request
/// instead, which is what makes the connection string's defaults mean anything.
#[derive(Clone, Default)]
pub(crate) struct Session {
    pub database: String,
    pub schema: String,
    pub warehouse: String,
    pub role: String,
}

/// What the API answers, for every request in the sequence.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Answer {
    /// The statement id, which is what a cancel names and what a partition is
    /// fetched by.
    #[serde(rename = "statementHandle")]
    pub handle: Option<String>,
    /// Snowflake's own code, as a string of digits with leading zeros —
    /// `090001` for success, `000604` for a statement somebody stopped. A string
    /// and not a number because that is how it arrives, and `000604` parsed as
    /// an integer is 604, which is a different thing to have to remember.
    pub code: Option<String>,
    pub message: Option<String>,
    #[serde(rename = "resultSetMetaData")]
    pub metadata: Option<ResultSetMetadata>,
    /// Row-major, and every value a JSON string; see `arrow_map`.
    pub data: Option<Vec<Vec<serde_json::Value>>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResultSetMetadata {
    /// The encoding of `data`. `jsonv2` is the only one there is, and
    /// `arrow_map` is written against it in every detail — so this is checked
    /// rather than ignored: a second encoding would otherwise arrive as a column
    /// of parse failures instead of as a sentence saying what happened.
    pub format: Option<String>,
    #[serde(rename = "rowType")]
    #[serde(default)]
    pub row_type: Vec<Column>,
    /// One entry per partition, the first of which arrived with this answer. A
    /// statement with no result set has none at all.
    ///
    /// What is *in* an entry — a row count and an uncompressed size — is not
    /// read, which is why this is not a struct: a partition is fetched by index
    /// and its rows are counted as they arrive, so the only thing that matters
    /// here is how many entries there are.
    #[serde(rename = "partitionInfo")]
    #[serde(default)]
    pub partitions: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Column {
    pub name: String,
    /// Snowflake's own type name, lower case: `fixed`, `text`, `timestamp_ltz`.
    /// Not the declared type a structure pane shows — that comes from
    /// `information_schema.columns`, which is about the table rather than about
    /// one result.
    #[serde(rename = "type")]
    pub kind: String,
    pub precision: Option<i64>,
    pub scale: Option<i64>,
}

/// One answer together with the status that carried it.
///
/// The status is half the message here, unlike Trino where every answer is a
/// `200`: `202` means the statement is still running and is not a failure, and
/// nothing inside the body distinguishes it from a `200` reliably.
pub(crate) struct Reply {
    pub status: StatusCode,
    pub answer: Answer,
}

/// One HTTPS client aimed at one Snowflake account.
///
/// Cheap to clone for the reason the Trino driver gives — `hyper_util`'s client
/// is a pool behind an `Arc` — which is what lets a cancel reach the account
/// while a fetch has the reader borrowed.
pub(crate) struct Wire {
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
    /// `https://account.snowflakecomputing.com`, with no trailing slash.
    origin: String,
    credential: Credential,
    session: Session,
}

impl Wire {
    pub fn new(origin: String, credential: Credential, session: Session) -> Self {
        // The provider is named rather than left to rustls' default, which is
        // whichever of `ring` and `aws-lc-rs` the workspace happens to have
        // compiled in — and which panics rather than fails when that is both or
        // neither. A connection dialog is a poor place to discover a feature
        // conflict.
        let tls = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
            .expect("ring is compiled in and its safe defaults are valid")
            .https_only()
            .enable_http1()
            .build();
        Self {
            client: Client::builder(TokioExecutor::new()).build(tls),
            origin,
            credential,
            session,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Starts `sql` and returns the answer that carries its handle.
    pub async fn post(&self, sql: &str) -> Result<Reply, SnowflakeError> {
        let mut body = json!({
            "statement": sql,
            // Zero, which the API reads as "no server-side ceiling". A statement
            // that runs for an hour is one the user is watching a progress
            // spinner for and can press Cancel on; a driver-imposed timeout
            // would end it with a message about a number nobody chose.
            "timeout": 0,
        });
        let object = body.as_object_mut().expect("a JSON object was just built");
        for (key, value) in [
            ("database", &self.session.database),
            ("schema", &self.session.schema),
            ("warehouse", &self.session.warehouse),
            ("role", &self.session.role),
        ] {
            // Absent rather than empty. An empty `warehouse` is not the same
            // request as no `warehouse`, and the API reads the first as a name
            // no account has.
            if !value.is_empty() {
                object.insert(key.to_string(), json!(value));
            }
        }
        let uri = format!("{}/api/v2/statements?async=true", self.origin);
        self.send(Method::POST, &uri, Full::new(Bytes::from(body.to_string())))
            .await
    }

    /// Asks whether a statement has finished, and takes its first partition if
    /// it has.
    pub async fn poll(&self, handle: &str) -> Result<Reply, SnowflakeError> {
        let uri = format!("{}/api/v2/statements/{handle}", self.origin);
        self.send(Method::GET, &uri, Full::default()).await
    }

    /// Fetches one partition of a finished result.
    pub async fn partition(&self, handle: &str, at: u64) -> Result<Reply, SnowflakeError> {
        let uri = format!("{}/api/v2/statements/{handle}?partition={at}", self.origin);
        self.send(Method::GET, &uri, Full::default()).await
    }

    /// Asks the account to abandon a statement, by handle.
    ///
    /// Best-effort, as the trait says. A statement that has already finished is
    /// not an error to cancel — the API answers with a code saying there was
    /// nothing to stop, and this reports success either way, because "the
    /// request was delivered" is what the caller was promised.
    pub async fn cancel(&self, handle: &str) -> Result<(), SnowflakeError> {
        let uri = format!("{}/api/v2/statements/{handle}/cancel", self.origin);
        let reply = self.send(Method::POST, &uri, Full::default()).await?;
        if reply.status.is_success() || reply.status == StatusCode::ACCEPTED {
            return Ok(());
        }
        Err(crate::failure(&reply.answer))
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        body: Full<Bytes>,
    ) -> Result<Reply, SnowflakeError> {
        let (token, kind) = self.credential.bearer()?;
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            // Not optional and not guessable from the token: the API takes both
            // kinds as a bearer, and this is what says which.
            .header("X-Snowflake-Authorization-Token-Type", kind)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            // What the account's query history shows this connection as. Free,
            // and the difference between a slow statement somebody can trace and
            // one attributed to nothing.
            .header("User-Agent", "dbclient/0.1")
            .body(body)
            .map_err(|e| SnowflakeError::Transport(format!("{uri}: {e}")))?;

        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| SnowflakeError::Transport(format!("{uri}: {e}")))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| SnowflakeError::Transport(format!("{uri}: reading the answer: {e}")))?
            .to_bytes();

        // A failure is JSON too — `{"code":"002003","message":"SQL compilation
        // error: …"}` — so it is parsed rather than turned into a status line.
        // What is not JSON at all is a proxy or a login page in the way, and
        // that is the case the raw body is kept for.
        match serde_json::from_slice::<Answer>(&bytes) {
            Ok(answer) => Ok(Reply { status, answer }),
            Err(_) if status.is_success() => Err(SnowflakeError::Transport(format!(
                "the account's answer did not parse: {}",
                String::from_utf8_lossy(&bytes).trim()
            ))),
            Err(_) => Err(SnowflakeError::Transport(format!(
                "the account answered {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            ))),
        }
    }
}
