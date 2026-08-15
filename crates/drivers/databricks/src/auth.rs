//! What a request proves itself with: a personal access token, or a service
//! principal's client credentials exchanged for one.
//!
//! Both end up as `Authorization: Bearer …` on every call, and the difference is
//! entirely in where the token comes from. A personal access token is carried as
//! it arrived. Machine-to-machine OAuth is one extra request — `POST
//! /oidc/v1/token` with the client id and secret in HTTP Basic and
//! `grant_type=client_credentials` in the body — whose answer is good for an
//! hour and is cached until it nearly is not.
//!
//! No workspace has answered any of this. The two pure parts are tested here —
//! the Basic header, and reading a token response with its expiry — and the part
//! that is not is whether Databricks issues a token for these credentials at
//! all. `wire.rs` performs the request, because the HTTP client lives there and
//! a second one would be a second connection pool.

use base64::Engine;
use serde::Deserialize;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::DatabricksError;

/// How long before expiry a cached token is replaced.
///
/// A token minted at the last moment can still be in flight when it expires.
/// Databricks issues these for an hour, so a minute of headroom costs one extra
/// exchange an hour and removes a race nobody would ever reproduce.
const RENEW_BEFORE: u64 = 60;

/// The token endpoint's path under the workspace host.
pub(crate) const TOKEN_PATH: &str = "/oidc/v1/token";

/// The body of a client-credentials request.
///
/// `all-apis` and not a narrower scope. The narrower ones exist — `sql`,
/// `dashboards.genie` — and this driver needs the SQL statement API, which is
/// under the umbrella scope on every workspace and under `sql` on some. Asking
/// for the one that is always there means a service principal configured the
/// ordinary way works; asking for the narrow one would fail on workspaces that
/// have not been told about it.
pub(crate) const CLIENT_CREDENTIALS: &str = "grant_type=client_credentials&scope=all-apis";

/// What this session proves itself with.
pub(crate) enum Credential {
    /// A personal access token, `dapi…`, carried as it arrived.
    Token(String),
    /// A service principal, exchanged for a token as needed. This is the
    /// "machine-to-machine" half of the phase's exit condition — no browser, no
    /// redirect, no device code.
    Machine(Machine),
}

/// A service principal and whatever token it last obtained.
pub(crate) struct Machine {
    /// The whole `Authorization` header value for the token request.
    basic: String,
    held: Mutex<Option<Held>>,
}

struct Held {
    token: String,
    /// The second this token stops being good.
    expires: u64,
}

impl Machine {
    pub fn new(client_id: &str, secret: &str) -> Self {
        Self {
            // HTTP Basic, which is where the secret goes: the token endpoint
            // also accepts the pair in the form body, and the header is the one
            // that does not end up in a proxy's access log alongside the grant
            // type.
            basic: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"))
            ),
            held: Mutex::new(None),
        }
    }

    pub fn authorization(&self) -> &str {
        &self.basic
    }

    /// The token last obtained, if it is still good.
    pub fn cached(&self, now: u64) -> Option<String> {
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        held.as_ref()
            .filter(|held| held.expires > now + RENEW_BEFORE)
            .map(|held| held.token.clone())
    }

    /// Keeps a token the workspace just issued.
    pub fn keep(&self, token: &str, lifetime: u64, now: u64) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        *held = Some(Held {
            token: token.to_string(),
            expires: now + lifetime,
        });
    }
}

/// What the token endpoint answers.
#[derive(Deserialize)]
struct Issued {
    access_token: String,
    /// Seconds. Optional in OAuth and absent from some issuers, which is why the
    /// caller has a default rather than an `unwrap`.
    expires_in: Option<u64>,
}

/// A token and its lifetime out of a token response.
///
/// The default lifetime is deliberately short. An issuer that says nothing about
/// expiry is one this driver knows nothing about, and treating its token as good
/// for an hour would mean an hour of `401`s if it was not; five minutes costs a
/// handful of extra exchanges and fails for at most that long.
pub(crate) fn issued(body: &[u8]) -> Result<(String, u64), DatabricksError> {
    let issued: Issued = serde_json::from_slice(body).map_err(|_| {
        DatabricksError::Auth(format!(
            "the token endpoint answered with something that is not a token: {}",
            String::from_utf8_lossy(body).trim()
        ))
    })?;
    Ok((issued.access_token, issued.expires_in.unwrap_or(300)))
}

/// Now, in seconds since the epoch.
pub(crate) fn now() -> Result<u64, DatabricksError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DatabricksError::Auth("this machine's clock is before 1970".to_string()))?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header the token endpoint reads the client id and secret out of.
    /// Getting the colon or the encoding wrong is a `401` with no detail in it.
    #[test]
    fn a_service_principal_is_offered_as_http_basic() {
        let machine = Machine::new("abc-123", "sekrit");
        assert_eq!(machine.authorization(), "Basic YWJjLTEyMzpzZWtyaXQ=");
    }

    /// A token is reused until it is nearly expired and then is not, which is
    /// the whole of the caching: the alternative is an exchange before every
    /// page of every statement.
    #[test]
    fn a_token_is_kept_until_it_is_nearly_expired() {
        let machine = Machine::new("abc", "sekrit");
        assert_eq!(machine.cached(1_000), None, "nothing has been issued yet");

        machine.keep("tok", 3600, 1_000);
        assert_eq!(machine.cached(1_000).as_deref(), Some("tok"));
        assert_eq!(machine.cached(4_000).as_deref(), Some("tok"));
        // Inside the renewal window: still valid, and deliberately not offered,
        // because a token that expires while the request is in flight fails in a
        // way nobody can reproduce.
        assert_eq!(machine.cached(4_560), None);
        assert_eq!(machine.cached(9_999), None);
    }

    /// The two shapes of answer: one that says how long it is good for, and one
    /// that does not.
    #[test]
    fn a_token_response_is_read_with_its_lifetime_or_a_short_default() {
        let (token, lifetime) =
            issued(br#"{"access_token":"tok","token_type":"Bearer","expires_in":3600}"#)
                .expect("a token");
        assert_eq!(token, "tok");
        assert_eq!(lifetime, 3600);

        let (_, lifetime) = issued(br#"{"access_token":"tok"}"#).expect("a token");
        assert_eq!(lifetime, 300, "an issuer that says nothing is not trusted");
    }

    /// An error page from a proxy, or an OAuth error, is a sentence rather than
    /// a parse failure nobody can act on.
    #[test]
    fn something_that_is_not_a_token_says_what_arrived_instead() {
        let message = issued(br#"{"error":"invalid_client"}"#)
            .expect_err("not a token")
            .to_string();
        assert!(message.contains("invalid_client"), "got: {message}");
    }
}
