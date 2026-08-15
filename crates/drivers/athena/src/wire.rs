//! The Athena API, which is one POST with a JSON body and a header naming the
//! action.
//!
//! **No server has answered any of this.** The action names, the request and
//! response shapes and the error envelope are read from the Athena API
//! reference.
//!
//! `POST /` to `athena.<region>.amazonaws.com` with `Content-Type:
//! application/x-amz-json-1.1` and `X-Amz-Target: AmazonAthena.<Action>`. There
//! is no other endpoint, no other verb and no path: which call this is lives
//! entirely in a header, which is why `Wire::call` takes the action as a string
//! and there is nothing else to build.
//!
//! **Six actions, and they divide into two jobs.** `StartQueryExecution`,
//! `GetQueryExecution`, `GetQueryResults` and `StopQueryExecution` run a
//! statement; `ListDatabases`, `ListTableMetadata` and `GetTableMetadata`
//! answer the navigator. The second group matters more than it looks: Athena's
//! catalog can also be read in SQL, with `SHOW DATABASES` and `DESCRIBE`, and
//! every one of those is a **query execution** — scanned bytes, a result file
//! written to S3, and a line on the bill. The metadata actions are ordinary API
//! calls that cost nothing. A navigator that expanded a database by running
//! `SHOW TABLES` would charge somebody for opening a tree.
//!
//! **No retry.** Athena answers a throttled request with
//! `ThrottlingException` and expects a backoff, and this does not do one: the
//! retry would happen inside a call the Cancel button cannot reach, so a
//! throttled account would show up as a client that has hung rather than as one
//! that failed. The exception reaches the caller instead. That is a gap and not
//! a decision to be proud of; it is stated here rather than discovered, exactly
//! as the Trino driver states the same gap about the same shape of problem.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::SystemTime;

use crate::AthenaError;
use crate::sigv4::{Signer, Unsigned};

/// The service name the signature is scoped to. Not the same string as the
/// target prefix below, and both are fixed by AWS.
const SERVICE: &str = "athena";

/// What `X-Amz-Target` is prefixed with. The API's own internal name, which is
/// older than the product's.
const TARGET: &str = "AmazonAthena";

/// The endpoint for one region.
///
/// AWS's own SDK resolves this through a rules engine that also covers FIPS
/// endpoints, dual-stack endpoints and the partitions whose DNS suffix is not
/// `amazonaws.com`. This builds the commercial name and the Chinese one, which
/// are the two that differ by a rule simple enough to be right; GovCloud shares
/// the commercial suffix and is therefore already covered, and a FIPS or
/// dual-stack endpoint is not reachable from this driver at all. That is a
/// limitation stated rather than hidden.
pub(crate) fn endpoint_host(region: &str) -> String {
    if region.starts_with("cn-") {
        return format!("{SERVICE}.{region}.amazonaws.com.cn");
    }
    format!("{SERVICE}.{region}.amazonaws.com")
}

/// One HTTPS client aimed at one region's Athena endpoint.
pub(crate) struct Wire {
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
    signer: Signer,
    host: String,
}

impl Wire {
    pub fn new(signer: Signer) -> Wire {
        let host = endpoint_host(&signer.region);
        // `https_only`, deliberately: a connector that would follow a plaintext
        // URL is one that will send a signed request in the clear the day a
        // redirect points somewhere unexpected — and an AWS signature carries
        // everything needed to replay the request it was made for.
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        Wire {
            client: Client::builder(TokioExecutor::new()).build(connector),
            signer,
            host,
        }
    }

    pub fn region(&self) -> &str {
        &self.signer.region
    }

    /// One action, signed and sent.
    ///
    /// The signature covers the body, so it has to be computed after the JSON is
    /// rendered and the rendered bytes have to be the ones sent — which is why
    /// this takes a `Value` and serializes it once rather than taking a
    /// serializable and letting hyper do it.
    pub async fn call<T: serde::de::DeserializeOwned>(
        &self,
        action: &str,
        body: serde_json::Value,
    ) -> Result<T, AthenaError> {
        let payload = body.to_string().into_bytes();
        let url = format!("https://{}/", self.host);
        let headers = self.signer.sign(
            &Unsigned {
                method: "POST",
                // Athena has one resource and it is the root. The canonical URI
                // is `/` rather than the empty string, which is a rule of SigV4
                // rather than of HTTP.
                path: "/",
                query: "",
                headers: vec![
                    ("host".to_string(), self.host.clone()),
                    (
                        "content-type".to_string(),
                        "application/x-amz-json-1.1".to_string(),
                    ),
                    ("x-amz-target".to_string(), format!("{TARGET}.{action}")),
                ],
                payload: &payload,
            },
            SystemTime::now(),
        );

        let mut builder = Request::builder().method(Method::POST).uri(&url);
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(Full::new(Bytes::from(payload)))
            .map_err(|e| AthenaError::Transport(format!("{action}: {e}")))?;

        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| AthenaError::Transport(crate::with_causes(&e, action)))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| AthenaError::Transport(format!("{action}: reading the answer: {e}")))?
            .to_bytes();

        if !status.is_success() {
            return Err(read_failure(action, status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            AthenaError::Transport(format!("{action}: Athena's answer did not parse: {e}"))
        })
    }
}

/// The error body every AWS JSON API answers a failure with.
///
/// `__type` is a shape name, sometimes bare and sometimes prefixed with a
/// namespace and a `#`; the last segment is the part worth showing. The message
/// arrives under `Message` or `message` depending on the service and sometimes
/// on the error, so both are read — a client that read one would show an empty
/// error banner for half the failures there are.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    #[serde(rename = "__type", default)]
    kind: String,
    #[serde(rename = "Message", default)]
    message_upper: String,
    #[serde(rename = "message", default)]
    message_lower: String,
}

fn read_failure(action: &str, status: StatusCode, bytes: &[u8]) -> AthenaError {
    match serde_json::from_slice::<ErrorEnvelope>(bytes) {
        Ok(envelope) if !envelope.kind.is_empty() || !envelope.text().is_empty() => {
            AthenaError::Query {
                // A shape with no message is a real answer — `AccessDenied` is
                // often sent bare — and the shape name is then the only thing
                // there is to show. Naming the action beside it is what turns
                // `AccessDeniedException` into something actionable.
                message: if envelope.text().is_empty() {
                    format!("{action}: {}", envelope.shape())
                } else {
                    envelope.text().to_string()
                },
                kind: envelope.shape().to_string(),
                position: None,
            }
        }
        _ => AthenaError::Transport(format!(
            "{action}: Athena answered {status}: {}",
            String::from_utf8_lossy(bytes).trim()
        )),
    }
}

impl ErrorEnvelope {
    fn text(&self) -> &str {
        if self.message_upper.is_empty() {
            &self.message_lower
        } else {
            &self.message_upper
        }
    }

    /// `InvalidRequestException` out of
    /// `com.amazon.athena#InvalidRequestException`.
    fn shape(&self) -> &str {
        self.kind.rsplit('#').next().unwrap_or(&self.kind)
    }
}

// ---------------------------------------------------------------------------
// What Athena answers with
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Started {
    pub query_execution_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Execution {
    pub query_execution: ExecutionDetail,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ExecutionDetail {
    /// `DDL`, `DML` or `UTILITY` — how Athena classifies what it just ran, and
    /// half of the rule that decides whether the first row of the first page is
    /// a header. See `crate::arrow_map::Plan::is_header` for the other half.
    #[serde(default)]
    pub statement_type: String,
    #[serde(default)]
    pub status: ExecutionStatus,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ExecutionStatus {
    /// `QUEUED`, `RUNNING`, `SUCCEEDED`, `FAILED` or `CANCELLED`.
    #[serde(default)]
    pub state: String,
    /// Why it is in that state, which for a failure is the engine's own message
    /// including the `line 1:35:` prefix a caret is read out of.
    #[serde(default)]
    pub state_change_reason: String,
    #[serde(default)]
    pub athena_error: Option<AthenaErrorDetail>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct AthenaErrorDetail {
    /// 1 for a system fault, 2 for the user's, 3 for something else's — which
    /// is the closest thing Athena has to a reason code.
    #[serde(default)]
    pub error_category: i32,
    #[serde(default)]
    pub error_message: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Results {
    #[serde(default)]
    pub result_set: ResultSet,
    #[serde(default)]
    pub next_token: String,
    /// Rows an `INSERT INTO` wrote. Absent for a read.
    #[serde(default)]
    pub update_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ResultSet {
    #[serde(default)]
    pub rows: Vec<Row>,
    #[serde(default)]
    pub result_set_metadata: ResultSetMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Row {
    #[serde(default)]
    pub data: Vec<Datum>,
}

/// One cell.
///
/// Every value in an Athena result is text, whatever the column's type — the
/// API has one field and it is called `VarCharValue`. A null is the field being
/// absent, which is why this is an `Option` and not an empty string: an empty
/// string is a real value that a `varchar` column can hold.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Datum {
    #[serde(default)]
    pub var_char_value: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ResultSetMetadata {
    #[serde(default)]
    pub column_info: Vec<ColumnInfo>,
}

/// A result's column, as Athena describes it.
///
/// The type here is **Presto's** — `varchar`, `integer`, `row` — which is not
/// the vocabulary `GetTableMetadata` answers in. That one is Hive's, because it
/// comes from the Glue catalog. The two are different names for the same
/// columns and this driver uses each where it belongs: this one to decide which
/// Arrow array a value goes into, and Hive's to show a person what the table
/// says it is. The Trino driver has the same split for the same reason.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ColumnInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub r#type: String,
    /// Digits, for a `decimal`. Also filled for the fixed-width numeric types,
    /// where it is their width rather than anything a client needs.
    #[serde(default)]
    pub precision: i64,
    #[serde(default)]
    pub scale: i64,
}

/// What `GetWorkGroup` answers with, in the parts `connect` reads.
///
/// Three facts and each of them decides something: whether the workgroup is
/// usable at all, whether it has already said where results go, and whether it
/// overrides a client that says otherwise.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct WorkGroupDetail {
    #[serde(default)]
    pub work_group: WorkGroup,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct WorkGroup {
    /// `ENABLED` or `DISABLED`. A disabled workgroup accepts no statement, and
    /// finding that out at connection time rather than on the first query is
    /// the reason `connect` makes this call.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub configuration: WorkGroupConfiguration,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct WorkGroupConfiguration {
    /// Whether the workgroup's own settings override whatever a client sends.
    #[serde(default)]
    pub enforce_work_group_configuration: bool,
    #[serde(default)]
    pub result_configuration: ResultConfiguration,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ResultConfiguration {
    #[serde(default)]
    pub output_location: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct DatabaseList {
    #[serde(default)]
    pub database_list: Vec<Database>,
    #[serde(default)]
    pub next_token: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Database {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct TableMetadataList {
    #[serde(default)]
    pub table_metadata_list: Vec<TableMetadata>,
    #[serde(default)]
    pub next_token: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct SingleTableMetadata {
    #[serde(default)]
    pub table_metadata: TableMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct TableMetadata {
    #[serde(default)]
    pub name: String,
    /// `EXTERNAL_TABLE`, `VIRTUAL_VIEW`, `MANAGED_TABLE` — Hive's words, not
    /// Athena's, because this comes out of the Glue catalog.
    #[serde(default)]
    pub table_type: String,
    #[serde(default)]
    pub columns: Vec<Column>,
    /// The partition columns, which Hive keeps apart from the rest and which are
    /// columns of the table in every way that matters to somebody reading it.
    #[serde(default)]
    pub partition_keys: Vec<Column>,
    /// Hive's free-form table properties. `comment`, `numRows`, and — for a
    /// view — the encoded definition.
    #[serde(default)]
    pub parameters: HashMap<String, String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Column {
    #[serde(default)]
    pub name: String,
    /// Hive's spelling: `int`, `bigint`, `string`, `struct<a:int>`.
    #[serde(default)]
    pub r#type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two DNS suffixes AWS actually uses for this service. GovCloud is
    /// already covered because it shares the commercial one; the partitions that
    /// do not are the Chinese ones.
    #[test]
    fn a_region_names_its_own_endpoint() {
        assert_eq!(endpoint_host("us-east-1"), "athena.us-east-1.amazonaws.com");
        assert_eq!(
            endpoint_host("us-gov-west-1"),
            "athena.us-gov-west-1.amazonaws.com"
        );
        assert_eq!(
            endpoint_host("cn-north-1"),
            "athena.cn-north-1.amazonaws.com.cn"
        );
    }

    /// The error envelope, read for the sentence a person acts on. `__type`
    /// carries a namespace on some services and not others, and only the last
    /// segment is worth showing.
    #[test]
    fn a_refusal_reaches_the_caller_as_what_athena_said() {
        let body = br#"{"__type":"com.amazon.athena#InvalidRequestException",
                        "Message":"line 1:35: mismatched input 'ORDER'"}"#;
        let error = read_failure("StartQueryExecution", StatusCode::BAD_REQUEST, body);
        assert!(error.to_string().contains("mismatched input"), "{error}");
        assert_eq!(error.kind(), Some("InvalidRequestException"));

        // The other spelling of the same field, which some AWS services use.
        let lower = br#"{"__type":"ThrottlingException","message":"Rate exceeded"}"#;
        let error = read_failure("GetQueryResults", StatusCode::BAD_REQUEST, lower);
        assert_eq!(error.to_string(), "Rate exceeded");
        assert_eq!(error.kind(), Some("ThrottlingException"));
    }

    /// A body that is not the envelope — a proxy in the way, an empty 503 — is
    /// reported as itself rather than as a complaint about JSON.
    #[test]
    fn an_answer_that_is_not_athenas_says_what_arrived() {
        let error = read_failure(
            "GetQueryResults",
            StatusCode::SERVICE_UNAVAILABLE,
            b"<html>gateway</html>",
        );
        let message = error.to_string();
        assert!(message.contains("503"), "{message}");
        assert!(message.contains("gateway"), "{message}");
    }

    /// A null is the field being absent, and an empty string is a value a
    /// `varchar` column can hold. Reading one as the other would put empty
    /// strings where the table has nulls, in every column of every result.
    #[test]
    fn an_absent_value_is_a_null_and_an_empty_one_is_not() {
        let row: Row =
            serde_json::from_str(r#"{"Data":[{"VarCharValue":"1"},{},{"VarCharValue":""}]}"#)
                .expect("a row");
        assert_eq!(row.data[0].var_char_value.as_deref(), Some("1"));
        assert_eq!(row.data[1].var_char_value, None);
        assert_eq!(row.data[2].var_char_value.as_deref(), Some(""));
    }
}
