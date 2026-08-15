//! Google's OAuth, done natively: a key on disk becomes a signed JWT becomes a
//! bearer token, and nothing here opens a browser.
//!
//! **No server has answered any of this.** What is checkable without one is
//! most of it, which is why this file has more tests than the rest of the
//! driver put together: the base64url alphabet, the PEM frame, the claim set,
//! where Application Default Credentials live, and when a cached token is too
//! old to reuse are all decided here and none of them needs Google to confirm
//! them. What needs Google is the last step — whether the token endpoint
//! accepts the assertion this builds — and that is the one thing below with no
//! test under it.
//!
//! **Two credential kinds, because Application Default Credentials are two
//! different files wearing one name.** A service-account key is
//! `"type": "service_account"` and carries an RSA private key; what `gcloud
//! auth application-default login` writes is `"type": "authorized_user"` and
//! carries a refresh token instead. They reach the same endpoint by different
//! grants, and a driver that read only the first would work for a CI robot and
//! fail on every developer laptop. Both are here.
//!
//! **The assertion flow rather than the three-legged one**, which is what makes
//! "no embedded browser" true rather than aspirational. `urn:ietf:params:oauth:
//! grant-type:jwt-bearer` is a POST with a JWT in it: the client proves it holds
//! the private key by signing, and there is no redirect, no consent screen and
//! no loopback listener anywhere in the path. The authorized-user case is the
//! refresh-token grant, which is the same shape — a POST with a secret in it —
//! and reuses the consent `gcloud` already obtained rather than asking again.
//!
//! **`ring` rather than a JWT library.** RS256 is one PKCS#1 v1.5 signature over
//! one string, `ring` is already in this tree under rustls, and the alternative
//! is a crate that would parse the JSON this file already parsed and build the
//! header this file already builds. What `ring` will not do is read a PKCS#1
//! (`BEGIN RSA PRIVATE KEY`) body, and Google has never issued one — every key
//! the console hands out is PKCS#8 — so that is refused by name rather than
//! silently mis-parsed.

use base64::Engine;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::BigQueryError;

/// Where the assertion goes, and where the refresh token goes.
///
/// A service-account key carries its own `token_uri` and this is only the
/// default for one that does not; the authorized-user file carries none at all,
/// so for that grant this is the address.
pub(crate) const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// What the token is asked to be good for.
///
/// One scope for both halves of this driver, which is a fact about Google's
/// scopes rather than a convenience: `…/auth/bigquery` covers the REST API that
/// submits the job *and* the Storage Read API that reads its rows, so there is
/// no second token and no moment where one half is authorised and the other is
/// not. `cloud-platform` would cover them too and would also cover every other
/// Google API the credential can reach, which is not what a database client
/// needs.
const SCOPE: &str = "https://www.googleapis.com/auth/bigquery";

/// How long the assertion claims to be good for.
///
/// One hour is Google's documented maximum for a JWT assertion, and asking for
/// less only means signing more often — the token that comes back has its own
/// lifetime and this number does not set it.
const ASSERTION_SECONDS: u64 = 3600;

/// How long before a token expires it stops being reused.
///
/// A token that is good for another two seconds is good for nothing: the
/// request carrying it has a network in front of it, and BigQuery counts the
/// expiry against the moment it reads the header rather than the moment this
/// side wrote it. Sixty seconds is enough for a slow round trip and short enough
/// that the refresh is not most of a token's life.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// Whatever is in an Application Default Credentials file.
#[derive(Debug, Clone)]
pub(crate) enum Credentials {
    /// A key the Google Cloud console issued for a service account.
    ServiceAccount(ServiceAccount),
    /// What `gcloud auth application-default login` leaves behind.
    ///
    /// Not a key: a refresh token, obtained interactively once by `gcloud` and
    /// spent here without asking again. That is the whole reason this driver can
    /// say it needs no embedded browser and still work on a laptop.
    User(UserCredentials),
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServiceAccount {
    pub client_email: String,
    /// The PEM as it sits in the JSON, newlines and all.
    pub private_key: String,
    /// The key's own id, which goes in the JWT header as `kid`. Optional in the
    /// file and therefore optional here; Google matches on the signature either
    /// way, and the `kid` is what lets it pick the right public key first try.
    #[serde(default)]
    pub private_key_id: String,
    /// Present in every key the console issues, and defaulted rather than
    /// required because the field is the endpoint's address and the endpoint has
    /// one well-known address.
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    /// The project the key belongs to, which is what makes `bigquery://` with no
    /// project in it openable at all.
    #[serde(default)]
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    /// `gcloud` writes this on a login that named a project; a plain
    /// `application-default login` does not.
    #[serde(default)]
    pub quota_project_id: String,
}

fn default_token_uri() -> String {
    TOKEN_URI.to_string()
}

impl Credentials {
    /// Reads a credentials file, whichever of the two kinds it is.
    ///
    /// Keyed on `type`, which is the only field both files have and the only one
    /// that says which they are. A file with neither shape is refused by name
    /// rather than by a missing-field error about whichever kind was tried
    /// first: the person holding it can see the word in it, and being told which
    /// two words are expected is what tells them what they picked up.
    pub fn parse(text: &str) -> Result<Credentials, BigQueryError> {
        #[derive(Deserialize)]
        struct Kind {
            #[serde(default)]
            r#type: String,
        }
        let kind: Kind = serde_json::from_str(text).map_err(|e| {
            BigQueryError::Credentials(format!("this credentials file is not JSON: {e}"))
        })?;
        match kind.r#type.as_str() {
            "service_account" => Ok(Credentials::ServiceAccount(
                serde_json::from_str(text).map_err(|e| {
                    BigQueryError::Credentials(format!(
                        "this service account key is missing something: {e}"
                    ))
                })?,
            )),
            "authorized_user" => Ok(Credentials::User(serde_json::from_str(text).map_err(
                |e| {
                    BigQueryError::Credentials(format!(
                        "these user credentials are missing something: {e}"
                    ))
                },
            )?)),
            other => Err(BigQueryError::Credentials(format!(
                "a credentials file says \"type\": \"service_account\" or \
                 \"type\": \"authorized_user\"; this one says {other:?}"
            ))),
        }
    }

    /// The project the credential belongs to, or empty where it names none.
    ///
    /// A connection string that names no project is completed from here, which
    /// is what makes `bigquery://` on its own openable. The user file's
    /// `quota_project_id` is not quite the same thing — it is which project gets
    /// billed for the API call rather than which holds the data — but on a
    /// laptop configured by `gcloud` they are the same project, and using it
    /// beats refusing to open.
    pub fn project(&self) -> &str {
        match self {
            Credentials::ServiceAccount(sa) => &sa.project_id,
            Credentials::User(user) => &user.quota_project_id,
        }
    }
}

/// Where Application Default Credentials are, in the order Google looks.
///
/// Two of the four places the ADC specification lists. `GOOGLE_APPLICATION
/// _CREDENTIALS` is the explicit one and `gcloud`'s well-known file is the one a
/// laptop has. The two that are missing are the metadata server, which answers
/// only inside Google Cloud and is not somewhere a desktop client runs, and the
/// App Engine legacy path, which is the same. A client that probed the metadata
/// server from a laptop would spend a connection timeout on every connect
/// proving it is not in a VM.
pub(crate) fn adc_path(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(explicit) = env("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    // The same path on macOS and Linux, which is worth stating because `gcloud`
    // puts it somewhere else on Windows and this build does not run there.
    let home = env("HOME").filter(|h| !h.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("gcloud")
            .join("application_default_credentials.json"),
    )
}

/// The body of a PEM block, decoded.
///
/// Refuses anything that is not `PRIVATE KEY`, and the refusal is the point:
/// `ring` takes PKCS#8 and a `RSA PRIVATE KEY` block is PKCS#1, which it would
/// reject with a message about the DER rather than about the file. Google has
/// never issued a PKCS#1 key, so this arm exists to name what somebody has
/// picked up by mistake — most likely a key they generated themselves with
/// `openssl genrsa`.
pub(crate) fn pkcs8_body(pem: &str) -> Result<Vec<u8>, BigQueryError> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    if pem.contains("BEGIN RSA PRIVATE KEY") {
        return Err(BigQueryError::Credentials(
            "this private key is in PKCS#1 form (BEGIN RSA PRIVATE KEY); a Google service \
             account key is PKCS#8 (BEGIN PRIVATE KEY)"
                .to_string(),
        ));
    }
    let start = pem.find(BEGIN).ok_or_else(|| {
        BigQueryError::Credentials(
            "this service account key has no BEGIN PRIVATE KEY line in its private_key".to_string(),
        )
    })? + BEGIN.len();
    let end = pem[start..].find(END).ok_or_else(|| {
        BigQueryError::Credentials(
            "this service account key's private_key has no END PRIVATE KEY line".to_string(),
        )
    })? + start;
    // Every kind of whitespace, because the PEM in the JSON has `\n` in it and
    // one pasted through a form may have `\r\n` or none at all.
    let body: String = pem[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| {
            BigQueryError::Credentials(format!(
                "this service account key's private_key is not base64: {e}"
            ))
        })
}

/// base64url without padding, which is what a JWT is written in.
///
/// Its own function because it is used three times and getting it wrong is
/// invisible: `+` and `/` decode perfectly well and produce a token Google
/// rejects with `invalid_grant`, which says nothing about the alphabet.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The two halves of a JWT that are signed, joined the way the signature covers
/// them.
///
/// Separate from the signing so that a test can look at it: the claim set is
/// where an expiry, an audience or a scope goes wrong, and all three are things
/// a person can read.
pub(crate) fn assertion_input(account: &ServiceAccount, issued_at: u64) -> String {
    // `kid` left out rather than sent empty when the key file has none. An empty
    // `kid` is a claim that the key has that id, and Google would look for it.
    let header = if account.private_key_id.is_empty() {
        serde_json::json!({ "alg": "RS256", "typ": "JWT" })
    } else {
        serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": account.private_key_id })
    };
    let claims = serde_json::json!({
        "iss": account.client_email,
        "scope": SCOPE,
        // The endpoint the assertion may be spent at, which is what stops one
        // captured in transit from being replayed somewhere else.
        "aud": account.token_uri,
        "iat": issued_at,
        "exp": issued_at + ASSERTION_SECONDS,
    });
    format!(
        "{}.{}",
        base64url(header.to_string().as_bytes()),
        base64url(claims.to_string().as_bytes())
    )
}

/// The whole assertion, signed.
pub(crate) fn assertion(account: &ServiceAccount, issued_at: u64) -> Result<String, BigQueryError> {
    let input = assertion_input(account, issued_at);
    let key = ring::signature::RsaKeyPair::from_pkcs8(&pkcs8_body(&account.private_key)?).map_err(
        |e| {
            BigQueryError::Credentials(format!(
                "this service account key is not a usable RSA key: {e}"
            ))
        },
    )?;
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| {
        BigQueryError::Credentials("this service account key would not sign".to_string())
    })?;
    Ok(format!("{input}.{}", base64url(&signature)))
}

/// The form body that turns a credential into a token.
///
/// Both grants in one place because they are the same request with different
/// fields, and because seeing them together is what shows that neither of them
/// is a browser.
pub(crate) fn grant(
    credentials: &Credentials,
    now: u64,
) -> Result<(String, String), BigQueryError> {
    match credentials {
        Credentials::ServiceAccount(account) => Ok((
            account.token_uri.clone(),
            form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion(account, now)?),
            ]),
        )),
        Credentials::User(user) => Ok((
            TOKEN_URI.to_string(),
            form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &user.client_id),
                ("client_secret", &user.client_secret),
                ("refresh_token", &user.refresh_token),
            ]),
        )),
    }
}

/// `application/x-www-form-urlencoded`, which is what the token endpoint takes.
///
/// Encoded rather than pasted: a JWT is base64url and needs none of it, but a
/// client secret is whatever Google generated and a refresh token contains `/`
/// often enough to matter.
fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| {
            format!(
                "{name}={}",
                percent_encoding::utf8_percent_encode(value, crate::UNRESERVED)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// What the token endpoint answers with.
#[derive(Debug, Deserialize)]
pub(crate) struct Grant {
    pub access_token: String,
    /// Seconds, and Google sends 3599 for a token it calls one hour. Defaulted
    /// rather than required so that a response without it is a token used once
    /// instead of a connection that fails.
    #[serde(default)]
    pub expires_in: u64,
}

/// A token and the moment it stops being worth sending.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub value: String,
    pub good_until: SystemTime,
}

impl Token {
    pub fn of(grant: Grant, now: SystemTime) -> Token {
        Token {
            value: grant.access_token,
            good_until: now + Duration::from_secs(grant.expires_in),
        }
    }

    /// Whether this token is worth sending at `now`.
    ///
    /// A margin rather than the expiry itself; see `EXPIRY_MARGIN`. A token with
    /// no stated lifetime is never fresh, which means it is fetched again for
    /// every request — wasteful, and the alternative is caching something whose
    /// expiry nobody stated.
    pub fn fresh_at(&self, now: SystemTime) -> bool {
        self.good_until
            .checked_sub(EXPIRY_MARGIN)
            .is_some_and(|deadline| now < deadline)
    }
}

/// Seconds since the epoch, for the `iat` and `exp` of an assertion.
pub(crate) fn unix_seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alphabet a JWT is written in, and the one thing about it that fails
    /// invisibly: standard base64 decodes fine and produces a token the endpoint
    /// refuses without saying why.
    #[test]
    fn a_jwt_segment_is_url_safe_and_unpadded() {
        // Bytes chosen so that standard base64 produces both `+` and `/`, and so
        // that the length is not a multiple of three and would otherwise be
        // padded.
        let awkward = [0xfb, 0xff, 0xbf, 0x00];
        let encoded = base64url(&awkward);
        assert!(!encoded.contains('+'), "{encoded}");
        assert!(!encoded.contains('/'), "{encoded}");
        assert!(!encoded.contains('='), "{encoded}");
        assert_eq!(encoded, "-_-_AA");
    }

    fn account() -> ServiceAccount {
        ServiceAccount {
            client_email: "reader@example.iam.gserviceaccount.com".to_string(),
            private_key: String::new(),
            private_key_id: "abc123".to_string(),
            token_uri: TOKEN_URI.to_string(),
            project_id: "example-project".to_string(),
        }
    }

    /// Every claim the assertion grant requires, and the two that decide whether
    /// it can be replayed: the audience it may be spent at and the hour it stops
    /// being good for.
    #[test]
    fn the_assertion_claims_what_the_grant_requires_and_expires_in_an_hour() {
        let input = assertion_input(&account(), 1_700_000_000);
        let (header, claims) = input.split_once('.').expect("two segments");
        let decode = |segment: &str| -> serde_json::Value {
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(segment)
                .expect("a JWT segment");
            serde_json::from_slice(&bytes).expect("JSON")
        };

        let header = decode(header);
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["kid"], "abc123");

        let claims = decode(claims);
        assert_eq!(claims["iss"], "reader@example.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], TOKEN_URI);
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["iat"], 1_700_000_000u64);
        assert_eq!(claims["exp"], 1_700_003_600u64);
    }

    /// A key file with no `private_key_id` must not claim to have one: an empty
    /// `kid` is a statement about the key, not the absence of a statement.
    #[test]
    fn a_key_with_no_id_sends_no_kid_rather_than_an_empty_one() {
        let mut account = account();
        account.private_key_id = String::new();
        let input = assertion_input(&account, 0);
        let header: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(input.split('.').next().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(header.get("kid").is_none(), "{header}");
    }

    /// The PEM frame, including the `\n`s that are in the JSON rather than in
    /// the file.
    #[test]
    fn a_pem_survives_the_newlines_the_json_put_in_it() {
        let der = [0x30u8, 0x82, 0x01, 0x2a];
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n");
        assert_eq!(pkcs8_body(&pem).expect("a body"), der);
        // The same thing with Windows line endings and with none at all.
        let crlf = format!("-----BEGIN PRIVATE KEY-----\r\n{body}\r\n-----END PRIVATE KEY-----");
        assert_eq!(pkcs8_body(&crlf).expect("a body"), der);
    }

    /// The mistake worth naming: a key generated by hand rather than issued by
    /// Google. `ring` would refuse it too, with a message about DER.
    #[test]
    fn a_pkcs1_key_is_refused_by_name() {
        let error =
            pkcs8_body("-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----")
                .expect_err("PKCS#1 is not what ring takes");
        assert!(error.to_string().contains("PKCS#8"), "{error}");
    }

    /// The two files that both call themselves Application Default Credentials.
    #[test]
    fn both_kinds_of_credentials_file_are_read_as_what_they_are() {
        let key = r#"{
            "type": "service_account",
            "project_id": "example-project",
            "private_key_id": "abc123",
            "private_key": "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
            "client_email": "reader@example.iam.gserviceaccount.com",
            "token_uri": "https://oauth2.googleapis.com/token"
        }"#;
        match Credentials::parse(key).expect("a service account") {
            Credentials::ServiceAccount(account) => {
                assert_eq!(
                    account.client_email,
                    "reader@example.iam.gserviceaccount.com"
                );
                assert_eq!(account.project_id, "example-project");
            }
            other => panic!("read as {other:?}"),
        }

        let login = r#"{
            "type": "authorized_user",
            "client_id": "764086051850.apps.googleusercontent.com",
            "client_secret": "d-secret",
            "refresh_token": "1//0gRefresh",
            "quota_project_id": "example-project"
        }"#;
        match Credentials::parse(login).expect("a user credential") {
            Credentials::User(user) => assert_eq!(user.refresh_token, "1//0gRefresh"),
            other => panic!("read as {other:?}"),
        }
    }

    /// A file that is neither says which two it should have been, because the
    /// person holding it can see the word that is in it.
    #[test]
    fn a_credentials_file_of_a_third_kind_says_which_two_are_expected() {
        let error = Credentials::parse(r#"{"type": "external_account"}"#)
            .expect_err("not a kind this driver reads");
        let message = error.to_string();
        assert!(message.contains("service_account"), "{message}");
        assert!(message.contains("authorized_user"), "{message}");
    }

    /// The explicit variable wins, and a laptop with neither still names the
    /// place `gcloud` writes to — so the failure is "that file is not there"
    /// rather than "no credentials", which are different problems.
    #[test]
    fn the_credentials_are_looked_for_where_google_looks_for_them() {
        let explicit = adc_path(|name| match name {
            "GOOGLE_APPLICATION_CREDENTIALS" => Some("/keys/robot.json".to_string()),
            "HOME" => Some("/Users/somebody".to_string()),
            _ => None,
        });
        assert_eq!(explicit, Some(PathBuf::from("/keys/robot.json")));

        let well_known = adc_path(|name| match name {
            "HOME" => Some("/Users/somebody".to_string()),
            _ => None,
        });
        assert_eq!(
            well_known,
            Some(PathBuf::from(
                "/Users/somebody/.config/gcloud/application_default_credentials.json"
            ))
        );

        assert_eq!(adc_path(|_| None), None);
    }

    /// The refresh grant carries a secret and a token, and both go through the
    /// encoder: a refresh token with a `/` in it — which is most of them — would
    /// otherwise arrive truncated at the endpoint.
    #[test]
    fn the_refresh_grant_encodes_the_secrets_it_carries() {
        let credentials = Credentials::User(UserCredentials {
            client_id: "764086051850.apps.googleusercontent.com".to_string(),
            client_secret: "d-se+cret".to_string(),
            refresh_token: "1//0gRefresh".to_string(),
            quota_project_id: String::new(),
        });
        let (uri, body) = grant(&credentials, 0).expect("a grant");
        assert_eq!(uri, TOKEN_URI);
        // The unreserved characters are left alone, so that a body somebody is
        // reading in a log still reads as one.
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=1%2F%2F0gRefresh"), "{body}");
        assert!(body.contains("client_secret=d-se%2Bcret"), "{body}");
    }

    /// A token is spent before it expires, not after: the request carrying it
    /// has a network in front of it.
    #[test]
    fn a_token_stops_being_reused_a_minute_before_it_expires() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let token = Token::of(
            Grant {
                access_token: "ya29.".to_string(),
                expires_in: 3599,
            },
            now,
        );
        assert!(token.fresh_at(now));
        assert!(token.fresh_at(now + Duration::from_secs(3538)));
        assert!(!token.fresh_at(now + Duration::from_secs(3540)));
        assert!(!token.fresh_at(now + Duration::from_secs(3600)));
    }

    /// A response with no lifetime in it is never treated as fresh, rather than
    /// being cached forever on a guess.
    #[test]
    fn a_token_with_no_stated_lifetime_is_not_cached() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let token = Token::of(
            Grant {
                access_token: "ya29.".to_string(),
                expires_in: 0,
            },
            now,
        );
        assert!(!token.fresh_at(now));
    }
}
