//! The SQL Statement Execution API, which is four requests and two shapes of
//! result.
//!
//! `POST /api/2.0/sql/statements` starts a statement and answers with an id.
//! `GET /api/2.0/sql/statements/<id>` says what state it is in and, once it has
//! succeeded, carries the first chunk of the result.
//! `GET /api/2.0/sql/statements/<id>/result/chunks/<n>` carries the rest.
//! `POST /api/2.0/sql/statements/<id>/cancel` stops it.
//!
//! **`wait_timeout` is `0s`, and that is a choice.** The API will hold the
//! request open for five to fifty seconds and answer with the result if the
//! statement finishes inside it, which is one round trip fewer. It is not taken,
//! for the reason the Snowflake driver gives about the same option: the
//! statement id is the only thing a cancel can name, and with the waiting form
//! it does not exist on this side until the statement it would stop has already
//! finished. A Cancel button that works on fast statements and not on slow ones
//! is worse than an extra round trip.
//!
//! **Two dispositions, and which one is used depends on who is asking.** A
//! statement run for its data asks for `EXTERNAL_LINKS` and `ARROW_STREAM`: the
//! warehouse writes Arrow to cloud storage and answers with presigned URLs, so
//! the bytes never pass through the control plane and never stop being Arrow.
//! That is the reason this driver is in this phase. A catalog query asks for
//! `INLINE` and `JSON_ARRAY` instead, because a dozen rows of table names are not
//! worth a second round trip to object storage — the same split the Trino driver
//! makes between `query` and `ask`.
//!
//! **A presigned link is fetched without the workspace's token**, which is the
//! one detail here that is not guessable. The URL already carries its own
//! signature in the query string, and S3 and Azure Blob refuse a request that
//! also carries an `Authorization` header. Sending the bearer along would work
//! against the control plane and fail against every result over a few megabytes.
//!
//! Nothing in this file has been sent to a Databricks workspace.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde_json::json;

use crate::DatabricksError;
use crate::auth::{self, CLIENT_CREDENTIALS, Credential, TOKEN_PATH};

/// How the result of a statement should be delivered.
///
/// Not an option a caller passes: the two are for the two kinds of statement
/// this driver runs, and mixing them up would either put a catalog query through
/// object storage or bring a hundred million rows through the control plane as
/// JSON.
#[derive(Clone, Copy)]
pub(crate) enum Delivery {
    /// Presigned links to Arrow, for a statement run for its data.
    Arrow,
    /// JSON in the answer itself, for a catalog query.
    Inline,
}

impl Delivery {
    fn disposition(self) -> (&'static str, &'static str) {
        match self {
            Delivery::Arrow => ("EXTERNAL_LINKS", "ARROW_STREAM"),
            Delivery::Inline => ("INLINE", "JSON_ARRAY"),
        }
    }

    /// What `manifest.format` must say for the answer to be readable.
    pub fn format(self) -> &'static str {
        self.disposition().1
    }
}

/// What the warehouse resolves unqualified names in, and which warehouse runs
/// the statement.
#[derive(Clone, Default)]
pub(crate) struct Session {
    /// Not optional: the API refuses a statement that names no warehouse, and
    /// there is no default.
    pub warehouse: String,
    pub catalog: String,
    pub schema: String,
}

/// What the API answers about a statement.
#[derive(Debug, Deserialize)]
pub(crate) struct Statement {
    pub statement_id: Option<String>,
    pub status: Option<Status>,
    pub manifest: Option<Manifest>,
    pub result: Option<Chunk>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Status {
    /// `PENDING`, `RUNNING`, `SUCCEEDED`, `FAILED`, `CANCELED`, `CLOSED`.
    pub state: String,
    pub error: Option<Failure>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Failure {
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Manifest {
    /// `ARROW_STREAM` or `JSON_ARRAY`, echoing what was asked for. Checked
    /// rather than ignored: a warehouse that answered with the other one would
    /// otherwise arrive as an Arrow decode failure on JSON, which says nothing
    /// about what happened.
    pub format: Option<String>,
    pub schema: Option<Columns>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Columns {
    #[serde(default)]
    pub columns: Vec<NamedColumn>,
}

/// One column as the manifest describes it.
///
/// Read only for a catalog query, whose values arrive as JSON and need a name to
/// be found by. A statement read for its data takes its schema off the Arrow
/// stream itself, which is the entire point of this driver — a type mapping here
/// would be a second opinion about columns Arrow has already described.
#[derive(Debug, Deserialize)]
pub(crate) struct NamedColumn {
    pub name: String,
}

/// One piece of a result: JSON rows, or links to Arrow.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Chunk {
    /// Row-major JSON, for an `INLINE` result.
    pub data_array: Option<Vec<Vec<serde_json::Value>>>,
    /// Presigned URLs, for an `EXTERNAL_LINKS` result. Usually one; the API
    /// allows several.
    #[serde(default)]
    pub external_links: Vec<Link>,
    /// Where the chunk after this one is, or `None` at the end of the result.
    pub next_chunk_internal_link: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Link {
    pub external_link: String,
    /// Present on the link rather than on the chunk for an `EXTERNAL_LINKS`
    /// result, which is why both are read.
    pub next_chunk_internal_link: Option<String>,
}

impl Chunk {
    /// The link to the next chunk, from wherever this answer put it.
    pub fn next(&self) -> Option<&str> {
        self.next_chunk_internal_link.as_deref().or_else(|| {
            self.external_links
                .last()
                .and_then(|link| link.next_chunk_internal_link.as_deref())
        })
    }
}

/// One HTTPS client aimed at one Databricks workspace.
///
/// The same client fetches the presigned result links, which are on somebody
/// else's host entirely — S3, Azure Blob, GCS. One pool rather than two, because
/// a `hyper_util` client is a pool per host inside one object and a second one
/// would only be a second set of idle connections.
pub(crate) struct Wire {
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
    /// `https://workspace-host`, with no trailing slash.
    origin: String,
    credential: Credential,
    session: Session,
}

impl Wire {
    pub fn new(origin: String, credential: Credential, session: Session) -> Self {
        // The provider is named rather than left to rustls' default, which is
        // whichever of `ring` and `aws-lc-rs` the workspace has compiled in and
        // which panics rather than fails when that is both or neither.
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

    /// Starts `sql` and returns the answer that carries its id.
    pub async fn post(&self, sql: &str, delivery: Delivery) -> Result<Statement, DatabricksError> {
        let (disposition, format) = delivery.disposition();
        let mut body = json!({
            "statement": sql,
            "warehouse_id": self.session.warehouse,
            // Asynchronous, always. See the module comment.
            "wait_timeout": "0s",
            "disposition": disposition,
            "format": format,
        });
        let object = body.as_object_mut().expect("a JSON object was just built");
        for (key, value) in [
            ("catalog", &self.session.catalog),
            ("schema", &self.session.schema),
        ] {
            // Absent rather than empty: the API reads an empty `catalog` as the
            // name of a catalog nobody has.
            if !value.is_empty() {
                object.insert(key.to_string(), json!(value));
            }
        }
        let uri = format!("{}/api/2.0/sql/statements", self.origin);
        self.call(Method::POST, &uri, Full::new(Bytes::from(body.to_string())))
            .await
    }

    /// Asks what state a statement is in, and takes its first chunk if it has
    /// finished.
    pub async fn poll(&self, id: &str) -> Result<Statement, DatabricksError> {
        let uri = format!("{}/api/2.0/sql/statements/{id}", self.origin);
        self.call(Method::GET, &uri, Full::default()).await
    }

    /// Follows one of the API's own internal chunk links.
    ///
    /// The link arrives as a path rather than a URL — `/api/2.0/sql/statements
    /// /…/result/chunks/1?row_offset=100` — so the workspace's origin goes in
    /// front of it. Written this way rather than composing the path from a chunk
    /// index, because the link carries a `row_offset` the API expects back and
    /// rebuilding it would be this driver deciding where the next chunk starts.
    pub async fn chunk(&self, link: &str) -> Result<Chunk, DatabricksError> {
        let uri = format!("{}{link}", self.origin);
        self.call(Method::GET, &uri, Full::default()).await
    }

    /// Fetches one presigned result link.
    ///
    /// **Without the workspace's token.** The URL is signed in its own query
    /// string, and cloud storage refuses a request that carries an
    /// `Authorization` header as well — so this is the one request in the driver
    /// that goes out unauthenticated, and it is unauthenticated on purpose.
    pub async fn fetch(&self, url: &str) -> Result<Bytes, DatabricksError> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Full::<Bytes>::default())
            .map_err(|e| DatabricksError::Transport(format!("fetching a result chunk: {e}")))?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| DatabricksError::Transport(format!("fetching a result chunk: {e}")))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| DatabricksError::Transport(format!("reading a result chunk: {e}")))?
            .to_bytes();
        if !status.is_success() {
            // The body rather than the status alone: cloud storage says
            // `AccessDenied` or `Request has expired` in XML, and which of the
            // two it is decides whether the fix is a permission or a slower
            // reader.
            return Err(DatabricksError::Transport(format!(
                "a result chunk answered {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }
        Ok(bytes)
    }

    /// Asks the warehouse to abandon a statement, by id.
    ///
    /// Best-effort, as the trait says. A statement that has already finished is
    /// not an error to cancel: the API answers that it could not be cancelled,
    /// and this reports success either way, because "the request was delivered"
    /// is what the caller was promised.
    pub async fn cancel(&self, id: &str) -> Result<(), DatabricksError> {
        let uri = format!("{}/api/2.0/sql/statements/{id}/cancel", self.origin);
        let _: serde_json::Value = self.call(Method::POST, &uri, Full::default()).await?;
        Ok(())
    }

    /// One authenticated call to the workspace, with its answer parsed.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        uri: &str,
        body: Full<Bytes>,
    ) -> Result<T, DatabricksError> {
        let token = self.token().await?;
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            // What the workspace's query history shows this connection as.
            .header("User-Agent", "dbclient/0.1")
            .body(body)
            .map_err(|e| DatabricksError::Transport(format!("{uri}: {e}")))?;

        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| DatabricksError::Transport(format!("{uri}: {e}")))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| DatabricksError::Transport(format!("{uri}: reading the answer: {e}")))?
            .to_bytes();

        if !status.is_success() {
            // A refusal here is about the request rather than about the
            // statement — a wrong warehouse id, an expired token, a workspace
            // that is not there. A statement that ran and failed answers `200`
            // with a state of `FAILED`, and `lib.rs` reads that.
            return Err(DatabricksError::Transport(format!(
                "the workspace answered {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            DatabricksError::Transport(format!("the workspace's answer did not parse: {e}"))
        })
    }

    /// The bearer token for the next request, obtaining one if needed.
    async fn token(&self) -> Result<String, DatabricksError> {
        let machine = match &self.credential {
            Credential::Token(token) => return Ok(token.clone()),
            Credential::Machine(machine) => machine,
        };
        let now = auth::now()?;
        if let Some(token) = machine.cached(now) {
            return Ok(token);
        }

        let uri = format!("{}{TOKEN_PATH}", self.origin);
        let request = Request::builder()
            .method(Method::POST)
            .uri(&uri)
            .header("Authorization", machine.authorization())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Full::new(Bytes::from_static(CLIENT_CREDENTIALS.as_bytes())))
            .map_err(|e| DatabricksError::Auth(format!("{uri}: {e}")))?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| DatabricksError::Auth(format!("{uri}: {e}")))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| DatabricksError::Auth(format!("{uri}: reading the answer: {e}")))?
            .to_bytes();
        if !status.is_success() {
            return Err(DatabricksError::Auth(format!(
                "the token endpoint answered {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }

        let (token, lifetime) = auth::issued(&bytes)?;
        machine.keep(&token, lifetime, now);
        Ok(token)
    }
}
