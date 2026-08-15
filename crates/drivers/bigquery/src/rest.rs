//! The JSON half of BigQuery: the token endpoint, the job endpoints and the
//! catalog.
//!
//! **No server has answered any of this.** The URLs, the field names and the
//! error shape are read from the BigQuery REST v2 reference; what nothing here
//! can establish is whether a real project answers them in the shape the
//! reference describes, and the places where that matters are named on the
//! functions themselves.
//!
//! **Rows never come through here, and that is the point of the driver.** This
//! file submits a job, waits for it to finish and asks where BigQuery put the
//! answer; `storage.rs` then reads that table as Arrow over gRPC. The REST API
//! has a perfectly good `jobs.getQueryResults` that would return the same rows
//! as JSON, and using it would mean every value being rendered to text by the
//! server and parsed back by this side — which is exactly the transcoding step
//! this driver exists to not have. So `getQueryResults` is never called, and the
//! only thing read out of the job is where its output landed.
//!
//! **`jobs.query` with `timeoutMs: 0` rather than `jobs.insert`.** The two
//! create the same job; `jobs.query` is one request where `jobs.insert` is a
//! request plus a configuration wrapper, and asking it to wait for zero
//! milliseconds turns it into "create this and tell me its id", which is all
//! this driver wants from it. Waiting inside the call was considered and
//! rejected: a query that takes ten seconds would spend them inside one HTTP
//! request that the Cancel button cannot reach, which is the same defect the
//! Trino driver's `wire.rs` names about retries.
//!
//! **`maxResults: 0` is not tidiness either.** Without it BigQuery attaches the
//! first page of rows to the response — as JSON, rendered, which is the
//! transcoding this driver refuses — and this side would then throw them away
//! having paid for them.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::Deserialize;
use std::time::SystemTime;

use crate::BigQueryError;
use crate::auth::{Credentials, Grant, Token, grant, unix_seconds};

/// The REST API's root. Fixed rather than configurable: BigQuery has one global
/// endpoint and the region a dataset lives in is carried as a job field rather
/// than in the hostname.
const API: &str = "https://bigquery.googleapis.com/bigquery/v2";

/// What has to be escaped in a path segment.
///
/// A dataset id is letters, digits and underscores and needs none of this, but a
/// table id may hold anything Unicode allows — BigQuery's own documentation
/// gives `表格` as a valid table name — and a `/` inside one would otherwise
/// address a different endpoint entirely.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'%')
    .add(b' ')
    .add(b'+');

fn segment(text: &str) -> String {
    utf8_percent_encode(text, SEGMENT).to_string()
}

/// Where a job is created.
pub(crate) fn queries_url(project: &str) -> String {
    format!("{API}/projects/{}/queries", segment(project))
}

/// Where a job's state is read.
///
/// The location is a query parameter and not optional in practice: a job created
/// in `europe-west4` is not found by a `jobs.get` that names no location, and
/// the answer is a 404 that reads as though the job never existed. The job
/// reference that came back from `jobs.query` carries it, so this driver always
/// has one to send.
pub(crate) fn job_url(project: &str, job: &str, location: &str) -> String {
    let base = format!("{API}/projects/{}/jobs/{}", segment(project), segment(job));
    if location.is_empty() {
        base
    } else {
        format!("{base}?location={}", segment(location))
    }
}

/// Where a job is asked to stop.
pub(crate) fn cancel_url(project: &str, job: &str, location: &str) -> String {
    let base = format!(
        "{API}/projects/{}/jobs/{}/cancel",
        segment(project),
        segment(job)
    );
    if location.is_empty() {
        base
    } else {
        format!("{base}?location={}", segment(location))
    }
}

/// One page of the datasets in a project.
pub(crate) fn datasets_url(project: &str, page: &str) -> String {
    // `all=true` so that hidden datasets — the ones whose id starts with `_`,
    // which is where BigQuery puts the anonymous tables query results land in —
    // are listed. Leaving them out was considered: they are noise in a
    // navigator. They are also where every query result this driver reads
    // actually lives, so a navigator that hid them would be hiding the one
    // dataset the user is most likely to be looking at when something has gone
    // wrong.
    let base = format!("{API}/projects/{}/datasets?all=true", segment(project));
    page_after(base, page)
}

/// The smallest request that proves a project is there and readable.
///
/// One dataset rather than none, because `maxResults=0` is not the same request
/// — the endpoint treats it as "unset" and answers with a full page. What is
/// wanted is the status code and not the body.
pub(crate) fn project_probe_url(project: &str) -> String {
    format!("{API}/projects/{}/datasets?maxResults=1", segment(project))
}

/// One page of the tables in a dataset.
pub(crate) fn tables_url(project: &str, dataset: &str, page: &str) -> String {
    let base = format!(
        "{API}/projects/{}/datasets/{}/tables",
        segment(project),
        segment(dataset)
    );
    page_after(base, page)
}

/// One table, with its schema and — for a view — its query text.
pub(crate) fn table_url(project: &str, dataset: &str, table: &str) -> String {
    format!(
        "{API}/projects/{}/datasets/{}/tables/{}",
        segment(project),
        segment(dataset),
        segment(table)
    )
}

fn page_after(base: String, page: &str) -> String {
    if page.is_empty() {
        return base;
    }
    let joiner = if base.contains('?') { '&' } else { '?' };
    format!("{base}{joiner}pageToken={}", segment(page))
}

// ---------------------------------------------------------------------------
// What BigQuery answers with
// ---------------------------------------------------------------------------

/// A job as `jobs.query` and `jobs.get` describe it.
///
/// Only the fields this driver reads. Everything else — and there is a great
/// deal of it, a finished job carries its own billing — is skipped by serde,
/// which is what keeps this readable against an API that grows fields.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Job {
    #[serde(default)]
    pub job_reference: JobReference,
    #[serde(default)]
    pub status: JobStatus,
    #[serde(default)]
    pub configuration: JobConfiguration,
    #[serde(default)]
    pub statistics: JobStatistics,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobReference {
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub job_id: String,
    /// The region the job ran in. Absent for `US`, present for everything else,
    /// and the reason `job_url` takes one.
    #[serde(default)]
    pub location: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobStatus {
    /// `PENDING`, `RUNNING` or `DONE`.
    #[serde(default)]
    pub state: String,
    /// The failure that ended the job, where one did.
    ///
    /// Distinct from `errors`, which also carries warnings a successful job
    /// produced — a job can finish with an `errors` array and no `errorResult`
    /// at all, and reporting those as failures would turn a successful query
    /// into an error banner.
    #[serde(default)]
    pub error_result: Option<Failure>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Failure {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobConfiguration {
    #[serde(default)]
    pub query: Option<QueryConfiguration>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueryConfiguration {
    /// Where BigQuery put the answer.
    ///
    /// For a `SELECT` with no destination of its own this is the anonymous
    /// table BigQuery caches the result in, and it is the whole reason this
    /// driver reads the job rather than the query response: it is the table the
    /// Storage Read API is then pointed at. A statement that produces no result
    /// set has none.
    #[serde(default)]
    pub destination_table: Option<TableReference>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TableReference {
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub dataset_id: String,
    #[serde(default)]
    pub table_id: String,
}

impl TableReference {
    /// The name the Storage Read API calls this table.
    pub fn resource(&self) -> String {
        format!(
            "projects/{}/datasets/{}/tables/{}",
            self.project_id, self.dataset_id, self.table_id
        )
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobStatistics {
    #[serde(default)]
    pub query: Option<QueryStatistics>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueryStatistics {
    /// Rows an `INSERT`, `UPDATE`, `DELETE` or `MERGE` changed.
    ///
    /// A string in the JSON, as every 64-bit integer in Google's REST APIs is —
    /// JSON's number is a double as far as most parsers are concerned, and a row
    /// count can exceed what one holds exactly. Parsed rather than trusted to
    /// serde's integer handling for that reason.
    #[serde(default)]
    pub num_dml_affected_rows: Option<String>,
}

/// A table as `tables.get` describes it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Table {
    #[serde(default)]
    pub schema: TableSchema,
    #[serde(default)]
    pub view: Option<ViewDefinition>,
    #[serde(default)]
    pub materialized_view: Option<ViewDefinition>,
    /// The unenforced key declarations, where the table has any.
    #[serde(default)]
    pub table_constraints: Option<TableConstraints>,
}

/// BigQuery's key declarations, which the planner may use and the storage layer
/// does not enforce.
///
/// "Unenforced" is not a caveat to be tucked away: a `PRIMARY KEY` here permits
/// duplicate rows, and a client that presented it as a constraint would be
/// telling somebody the database is checking something it is not. `metadata.rs`
/// reports them as declared and says so on the method.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TableConstraints {
    #[serde(default)]
    pub primary_key: Option<PrimaryKey>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PrimaryKey {
    #[serde(default)]
    pub columns: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForeignKey {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub referenced_table: TableReference,
    #[serde(default)]
    pub column_references: Vec<ColumnReference>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ColumnReference {
    #[serde(default)]
    pub referencing_column: String,
    #[serde(default)]
    pub referenced_column: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TableSchema {
    #[serde(default)]
    pub fields: Vec<TableField>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TableField {
    #[serde(default)]
    pub name: String,
    /// `STRING`, `INT64`, `NUMERIC`, `RECORD` — the legacy spelling, which is
    /// what this endpoint returns whatever the table was created with.
    #[serde(default)]
    pub r#type: String,
    /// `NULLABLE`, `REQUIRED` or `REPEATED`.
    #[serde(default)]
    pub mode: String,
    /// Present and non-empty for a `RECORD`.
    #[serde(default)]
    pub fields: Vec<TableField>,
    /// The `precision` and `scale` of a `NUMERIC`, as strings.
    #[serde(default)]
    pub precision: Option<String>,
    #[serde(default)]
    pub scale: Option<String>,
    #[serde(default)]
    pub max_length: Option<String>,
    #[serde(default)]
    pub default_value_expression: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ViewDefinition {
    #[serde(default)]
    pub query: String,
}

/// One page of `datasets.list`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DatasetPage {
    #[serde(default)]
    pub datasets: Vec<DatasetEntry>,
    #[serde(default)]
    pub next_page_token: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DatasetEntry {
    #[serde(default)]
    pub dataset_reference: DatasetReference,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DatasetReference {
    #[serde(default)]
    pub dataset_id: String,
}

/// One page of `tables.list`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TablePage {
    #[serde(default)]
    pub tables: Vec<TableEntry>,
    #[serde(default)]
    pub next_page_token: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TableEntry {
    #[serde(default)]
    pub table_reference: TableReference,
    #[serde(default)]
    pub r#type: String,
}

/// The error body every Google API answers a failure with.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    errors: Vec<Failure>,
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// One HTTPS client aimed at the BigQuery REST API, with the credential it
/// signs requests with.
///
/// Cheap to clone in the sense that matters — `hyper_util`'s client is a pool
/// behind an `Arc` — but held behind an `Arc` here rather than cloned, because
/// the token cache must be shared: two clones would each fetch their own token
/// and each pay the round trip.
pub(crate) struct Api {
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
    credentials: Credentials,
    /// The access token in force, if one is. `tokio`'s mutex and not the
    /// standard one: the guard is held across the fetch, which is an await.
    token: tokio::sync::Mutex<Option<Token>>,
}

impl Api {
    pub fn new(credentials: Credentials) -> Api {
        // `https_only`, deliberately: every endpoint this driver talks to is
        // HTTPS, and a connector that would follow a plaintext URL is one that
        // will send a bearer token in the clear the day a redirect points
        // somewhere unexpected.
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        Api {
            client: Client::builder(TokioExecutor::new()).build(connector),
            credentials,
            token: tokio::sync::Mutex::new(None),
        }
    }

    /// A bearer token good for now, fetching one if the cached one is not.
    ///
    /// The guard is held across the fetch on purpose. Two statements starting at
    /// once would otherwise each see no token and each spend a round trip
    /// getting one, and Google counts those.
    pub async fn token(&self) -> Result<String, BigQueryError> {
        let now = SystemTime::now();
        let mut held = self.token.lock().await;
        if let Some(token) = held.as_ref().filter(|t| t.fresh_at(now)) {
            return Ok(token.value.clone());
        }

        let (uri, body) = grant(&self.credentials, unix_seconds(now))?;
        let request = Request::builder()
            .method(Method::POST)
            .uri(&uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| BigQueryError::Transport(format!("{uri}: {e}")))?;
        let (status, bytes) = self.send(request, &uri).await?;
        if status != StatusCode::OK {
            // The body rather than the status: the token endpoint answers a
            // clock skew, a revoked key and a wrong audience all with 400, and
            // only the body says which.
            return Err(BigQueryError::Credentials(format!(
                "the token endpoint answered {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }
        let granted: Grant = serde_json::from_slice(&bytes).map_err(|e| {
            BigQueryError::Credentials(format!("the token endpoint's answer did not parse: {e}"))
        })?;
        let token = Token::of(granted, now);
        let value = token.value.clone();
        *held = Some(token);
        Ok(value)
    }

    /// A `GET` that answers with JSON.
    pub async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, BigQueryError> {
        self.json(Method::GET, url, None).await
    }

    /// A `POST` that answers with JSON.
    pub async fn post<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: serde_json::Value,
    ) -> Result<T, BigQueryError> {
        self.json(Method::POST, url, Some(body)).await
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, BigQueryError> {
        let token = self.token().await?;
        let mut builder = Request::builder()
            .method(method)
            .uri(url)
            .header("authorization", format!("Bearer {token}"))
            // What the project's audit log shows this connection as. Free, and
            // the difference between an expensive query somebody can trace and
            // one attributed to nothing.
            .header("user-agent", crate::CLIENT_NAME);
        let payload = match body {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                Full::new(Bytes::from(value.to_string()))
            }
            None => Full::default(),
        };
        let request = builder
            .body(payload)
            .map_err(|e| BigQueryError::Transport(format!("{url}: {e}")))?;

        let (status, bytes) = self.send(request, url).await?;
        if !status.is_success() {
            return Err(read_failure(status, &bytes));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| BigQueryError::Transport(format!("BigQuery's answer did not parse: {e}")))
    }

    async fn send(
        &self,
        request: Request<Full<Bytes>>,
        url: &str,
    ) -> Result<(StatusCode, Bytes), BigQueryError> {
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| BigQueryError::Transport(crate::with_causes(&e, url)))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| BigQueryError::Transport(format!("{url}: reading the answer: {e}")))?
            .to_bytes();
        Ok((status, bytes))
    }
}

/// A failed HTTP response as the sentence a person should read.
///
/// The envelope rather than the status, for the reason the Trino driver's `send`
/// gives: a 400 from BigQuery says which part of the statement it disliked and
/// a 403 says which permission is missing, and neither is guessable from the
/// number. A body that is not the envelope — a proxy's HTML, an empty 502 — is
/// reported as itself rather than as a parse failure about JSON.
fn read_failure(status: StatusCode, bytes: &[u8]) -> BigQueryError {
    match serde_json::from_slice::<ErrorEnvelope>(bytes) {
        Ok(envelope) => {
            let detail = envelope
                .error
                .errors
                .first()
                .filter(|e| !e.message.is_empty() && e.message != envelope.error.message)
                .map(|e| format!(": {}", e.message))
                .unwrap_or_default();
            BigQueryError::Query {
                message: format!("{}{detail}", envelope.error.message),
                reason: envelope
                    .error
                    .errors
                    .first()
                    .map(|e| e.reason.clone())
                    .unwrap_or(envelope.error.status),
                // Resolved by whoever knows the statement; see
                // `BigQueryError::about`. A metadata request has one this driver
                // wrote and the user never saw, and a caret in text nobody typed
                // is worse than none anywhere.
                position: None,
            }
        }
        Err(_) => BigQueryError::Transport(format!(
            "BigQuery answered {status}: {}",
            String::from_utf8_lossy(bytes).trim()
        )),
    }
}

/// A 64-bit count that arrived as a string.
pub(crate) fn count(text: &Option<String>) -> Option<u64> {
    text.as_ref()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address every request in this file is built from, and the two things
    /// that go wrong invisibly: a missing location turns `jobs.get` into a 404
    /// that reads as a job that never existed, and an unencoded segment
    /// addresses a different endpoint.
    #[test]
    fn a_job_is_addressed_by_its_id_and_the_region_it_ran_in() {
        assert_eq!(
            job_url("example-project", "job_abc123", "europe-west4"),
            "https://bigquery.googleapis.com/bigquery/v2/projects/example-project\
             /jobs/job_abc123?location=europe-west4"
        );
        // `US` comes back as an empty location, and a URL with `location=` and
        // nothing after it is not the same request as one without the parameter.
        assert_eq!(
            job_url("example-project", "job_abc123", ""),
            "https://bigquery.googleapis.com/bigquery/v2/projects/example-project/jobs/job_abc123"
        );
        assert_eq!(
            cancel_url("example-project", "job_abc123", "US"),
            "https://bigquery.googleapis.com/bigquery/v2/projects/example-project\
             /jobs/job_abc123/cancel?location=US"
        );
    }

    /// The probe a connection lives or dies by. It asks the project this
    /// connection names, not the credential's own, because those differ whenever
    /// one service account reads someone else's data — and a probe against the
    /// wrong project would pass while the connection is unusable.
    #[test]
    fn the_connection_probe_asks_the_project_the_url_named() {
        assert_eq!(
            project_probe_url("my-project-123"),
            "https://bigquery.googleapis.com/bigquery/v2/projects/my-project-123\
             /datasets?maxResults=1"
        );
    }

    /// A table id may hold anything Unicode allows, and a `/` in one would
    /// address a different resource entirely.
    #[test]
    fn a_name_with_a_delimiter_in_it_stays_one_path_segment() {
        assert_eq!(
            table_url("p", "d", "a/b"),
            "https://bigquery.googleapis.com/bigquery/v2/projects/p/datasets/d/tables/a%2Fb"
        );
        assert_eq!(
            table_url("p", "d", "表格"),
            "https://bigquery.googleapis.com/bigquery/v2/projects/p/datasets/d/tables/\
             %E8%A1%A8%E6%A0%BC"
        );
    }

    /// The page token joins with `&` where there is already a parameter and with
    /// `?` where there is not — the datasets listing has one and the tables
    /// listing does not, which is exactly the pair that would be got wrong.
    #[test]
    fn a_page_token_joins_whichever_way_the_url_it_continues_needs() {
        assert_eq!(
            datasets_url("p", "tok/en"),
            "https://bigquery.googleapis.com/bigquery/v2/projects/p/datasets\
             ?all=true&pageToken=tok%2Fen"
        );
        assert_eq!(
            tables_url("p", "d", "tok"),
            "https://bigquery.googleapis.com/bigquery/v2/projects/p/datasets/d/tables?pageToken=tok"
        );
        assert_eq!(
            tables_url("p", "d", ""),
            "https://bigquery.googleapis.com/bigquery/v2/projects/p/datasets/d/tables"
        );
    }

    /// The error envelope, read for the sentence a person acts on rather than
    /// the status code they cannot.
    #[test]
    fn a_refusal_reaches_the_caller_as_what_bigquery_said() {
        let body = br#"{"error":{"code":400,
            "message":"Syntax error: Unexpected keyword ORDER at [1:38]",
            "status":"INVALID_ARGUMENT",
            "errors":[{"reason":"invalidQuery","location":"query",
                       "message":"Syntax error: Unexpected keyword ORDER at [1:38]"}]}}"#;
        let error = read_failure(StatusCode::BAD_REQUEST, body);
        assert!(
            error.to_string().contains("Unexpected keyword ORDER"),
            "{error}"
        );
        assert_eq!(error.reason(), Some("invalidQuery"));
    }

    /// A body that is not the envelope — a proxy in the way, an empty 502 — is
    /// reported as itself rather than as a complaint about JSON.
    #[test]
    fn an_answer_that_is_not_bigquerys_says_what_arrived() {
        let error = read_failure(StatusCode::BAD_GATEWAY, b"<html>gateway</html>");
        let message = error.to_string();
        assert!(message.contains("502"), "{message}");
        assert!(message.contains("gateway"), "{message}");
    }

    /// Every 64-bit number in a Google REST API is a string, because JSON's
    /// number is a double as far as most parsers are concerned.
    #[test]
    fn a_row_count_that_arrived_as_text_is_read_as_a_number() {
        assert_eq!(
            count(&Some("9007199254740993".to_string())),
            Some(9007199254740993)
        );
        assert_eq!(count(&None), None);
        assert_eq!(count(&Some(String::new())), None);
    }
}
