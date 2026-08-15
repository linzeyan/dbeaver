//! AWS Signature Version 4, which is what stands in for a password here.
//!
//! **No server has answered this driver.** This file is the part of it least
//! affected by that, and deliberately so: SigV4 is a pure function from a
//! request, a key and an instant to a header, AWS publishes worked examples of
//! every intermediate step, and the tests below are those examples. A signature
//! that matches the published one for the published request is not evidence
//! about Athena, but it is evidence about the signer — which is the piece
//! everything else here depends on and the piece that fails silently, because a
//! wrong signature comes back as `SignatureDoesNotMatch` and no clue about
//! which of the four steps was wrong.
//!
//! **Written out rather than taken from `aws-sigv4`.** The AWS SDK for Rust is
//! an excellent thing and it is also thirty crates: `aws-config`,
//! `aws-smithy-runtime`, `aws-smithy-http`, a generated client per service and
//! their shared runtime. What this driver needs from all of that is one
//! `Authorization` header, and the algorithm below is sixty lines over two
//! primitives that are already in this tree under rustls. The trade is stated
//! here so that it can be revisited: the day this driver needs assumed roles,
//! SSO, IMDS or web identity, the SDK's credential chain is worth more than
//! sixty lines and this decision should be reversed rather than extended.
//!
//! **What is signed is the whole request and not a token**, which is the
//! property worth knowing: a captured Athena request cannot be replayed against
//! a different action, a different body or a different day, because all three
//! are inside the hash.

use ring::{digest, hmac};
use std::time::SystemTime;

/// The only algorithm this driver signs with. AWS also defines
/// `AWS4-ECDSA-P256-SHA256`, which exists for requests that must be valid in
/// more than one region at once and which Athena has no use for.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// What the derived key is terminated with, which is what stops one region's
/// key from signing another region's request.
const TERMINATOR: &str = "aws4_request";

/// One set of AWS credentials, aimed at one region and one service.
#[derive(Debug, Clone)]
pub(crate) struct Signer {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Present for temporary credentials — an assumed role, an SSO session,
    /// anything `aws sts` handed out. It is a *header* rather than part of the
    /// signature's arithmetic, and it is signed like any other header, which is
    /// why leaving it out of `headers` would produce a signature that does not
    /// match the request that carries it.
    pub session_token: Option<String>,
    pub region: String,
    pub service: String,
}

/// One request, in the parts a signature is computed over.
pub(crate) struct Unsigned<'a> {
    pub method: &'a str,
    /// Already normalised and percent-encoded. Athena's is always `/`.
    pub path: &'a str,
    /// Already canonical: sorted, encoded, `&`-joined. Athena's is always empty.
    pub query: &'a str,
    /// Header names in any case and any order; this sorts and folds them.
    pub headers: Vec<(String, String)>,
    pub payload: &'a [u8],
}

impl Signer {
    /// The headers that turn `request` into a signed one.
    ///
    /// Returns what to add rather than mutating, so that the caller keeps one
    /// place where a request is assembled and this keeps one place where it is
    /// signed.
    pub fn sign(&self, request: &Unsigned<'_>, at: SystemTime) -> Vec<(String, String)> {
        let instant = timestamp(at);
        let day = &instant[..8];

        let mut headers = request.headers.clone();
        headers.push(("x-amz-date".to_string(), instant.clone()));
        if let Some(token) = &self.session_token {
            headers.push(("x-amz-security-token".to_string(), token.clone()));
        }

        let (canonical, signed_names) = canonical_request(
            request.method,
            request.path,
            request.query,
            &headers,
            request.payload,
        );
        let scope = format!("{day}/{}/{}/{TERMINATOR}", self.region, self.service);
        let to_sign = string_to_sign(&instant, &scope, &canonical);
        let key = signing_key(&self.secret_access_key, day, &self.region, &self.service);
        let signature =
            hex(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), to_sign.as_bytes()).as_ref());

        let authorization = format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}",
            self.access_key_id
        );
        headers.push(("authorization".to_string(), authorization));
        headers
    }
}

/// The canonical request, and the `;`-joined names it signed.
///
/// The five rules that matter, all of which produce a wrong signature silently
/// if broken: the header names are lower-cased, their values have leading and
/// trailing whitespace trimmed, the list is sorted by name, every entry ends
/// with a newline *including the last*, and the block is followed by a blank
/// line before the signed-name list.
///
/// Sequential internal spaces in a header value are also supposed to be
/// collapsed to one. Nothing this driver sends has any — the values are an
/// action name, a host and a content type — and collapsing them is left out
/// rather than written and never exercised.
fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload: &[u8],
) -> (String, String) {
    let mut folded: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    folded.sort_by(|a, b| a.0.cmp(&b.0));

    let mut canonical = String::new();
    canonical.push_str(method);
    canonical.push('\n');
    canonical.push_str(path);
    canonical.push('\n');
    canonical.push_str(query);
    canonical.push('\n');
    for (name, value) in &folded {
        canonical.push_str(name);
        canonical.push(':');
        canonical.push_str(value);
        canonical.push('\n');
    }
    canonical.push('\n');
    let names = folded
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    canonical.push_str(&names);
    canonical.push('\n');
    canonical.push_str(&hex(digest::digest(&digest::SHA256, payload).as_ref()));
    (canonical, names)
}

/// The four lines that are actually signed.
fn string_to_sign(instant: &str, scope: &str, canonical: &str) -> String {
    format!(
        "{ALGORITHM}\n{instant}\n{scope}\n{}",
        hex(digest::digest(&digest::SHA256, canonical.as_bytes()).as_ref())
    )
}

/// The key, derived once per day per region per service.
///
/// Four chained HMACs, and the chain is the whole security argument: the key
/// that signs a request is not the secret, it is a value derived from the secret
/// that is useless outside one day, one region and one service. A signature
/// captured from an Athena request in `us-east-1` cannot be replayed against S3
/// or against tomorrow.
fn signing_key(secret: &str, day: &str, region: &str, service: &str) -> Vec<u8> {
    let mut key = format!("AWS4{secret}").into_bytes();
    for step in [day, region, service, TERMINATOR] {
        key = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), step.as_bytes())
            .as_ref()
            .to_vec();
    }
    key
}

/// `20150830T123600Z`, which is the only format AWS accepts for `x-amz-date`.
///
/// The scope's date is the first eight characters of this rather than a second
/// formatting, because the two must agree: a request stamped just before
/// midnight and scoped to the following day is refused, and deriving one from
/// the other is what makes that impossible rather than unlikely.
fn timestamp(at: SystemTime) -> String {
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    chrono::DateTime::from_timestamp(seconds as i64, 0)
        .unwrap_or_default()
        .format("%Y%m%dT%H%M%SZ")
        .to_string()
}

/// Lower-case hex, which is what every step of SigV4 is written in.
///
/// Its own function rather than a dependency: it is four lines, and the one
/// thing that goes wrong — upper case — produces a signature that is refused
/// with no clue as to why.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// AWS's own worked example, which is the strongest thing this driver can
    /// be checked against without an account.
    ///
    /// The request, the credentials and every intermediate value are the ones
    /// published in the Signature Version 4 documentation — a `GET` to the IAM
    /// endpoint, signed at `20150830T123600Z` with the example key. Each step is
    /// asserted separately rather than only the final signature, because the
    /// four steps fail identically when they fail: a mismatched signature says
    /// nothing about which of them was wrong.
    const ACCESS_KEY: &str = "AKIDEXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn example_headers() -> Vec<(String, String)> {
        vec![
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            ("Host".to_string(), "iam.amazonaws.com".to_string()),
            ("X-Amz-Date".to_string(), "20150830T123600Z".to_string()),
        ]
    }

    #[test]
    fn the_canonical_request_is_the_one_aws_publishes() {
        let (canonical, names) = canonical_request(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &example_headers(),
            b"",
        );
        assert_eq!(
            canonical,
            "GET\n\
             /\n\
             Action=ListUsers&Version=2010-05-08\n\
             content-type:application/x-www-form-urlencoded; charset=utf-8\n\
             host:iam.amazonaws.com\n\
             x-amz-date:20150830T123600Z\n\
             \n\
             content-type;host;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(names, "content-type;host;x-amz-date");
    }

    #[test]
    fn the_string_to_sign_is_the_one_aws_publishes() {
        let (canonical, _) = canonical_request(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &example_headers(),
            b"",
        );
        let to_sign = string_to_sign(
            "20150830T123600Z",
            "20150830/us-east-1/iam/aws4_request",
            &canonical,
        );
        assert_eq!(
            to_sign,
            "AWS4-HMAC-SHA256\n\
             20150830T123600Z\n\
             20150830/us-east-1/iam/aws4_request\n\
             f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
    }

    #[test]
    fn the_signature_is_the_one_aws_publishes() {
        let (canonical, _) = canonical_request(
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &example_headers(),
            b"",
        );
        let to_sign = string_to_sign(
            "20150830T123600Z",
            "20150830/us-east-1/iam/aws4_request",
            &canonical,
        );
        let key = signing_key(SECRET_KEY, "20150830", "us-east-1", "iam");
        let signature =
            hex(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), to_sign.as_bytes()).as_ref());
        assert_eq!(
            signature,
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    /// The header the whole file exists to produce, assembled the way AWS parses
    /// it back: the three parts in that order, separated by `, `.
    #[test]
    fn the_authorization_header_names_the_key_the_scope_and_the_headers() {
        let signer = Signer {
            access_key_id: ACCESS_KEY.to_string(),
            secret_access_key: SECRET_KEY.to_string(),
            session_token: None,
            region: "us-east-1".to_string(),
            service: "iam".to_string(),
        };
        let headers = signer.sign(
            &Unsigned {
                method: "GET",
                path: "/",
                query: "Action=ListUsers&Version=2010-05-08",
                headers: vec![
                    (
                        "Content-Type".to_string(),
                        "application/x-www-form-urlencoded; charset=utf-8".to_string(),
                    ),
                    ("Host".to_string(), "iam.amazonaws.com".to_string()),
                ],
                payload: b"",
            },
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160),
        );
        let authorization = headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str())
            .expect("an authorization header");
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    /// The instant AWS's example is signed at, so that the whole signature can be
    /// reached through the public entry point and not only through its parts.
    #[test]
    fn the_timestamp_is_the_only_shape_aws_accepts() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160);
        assert_eq!(timestamp(at), "20150830T123600Z");
        // And the scope's day is the first eight characters of it, which is what
        // stops a request stamped at 23:59:59 from being scoped to tomorrow.
        assert_eq!(&timestamp(at)[..8], "20150830");
    }

    /// A temporary credential's token is signed like any other header. Leaving
    /// it out of the signature would produce a request whose header set does not
    /// match what was signed, which AWS refuses with the same message as a wrong
    /// secret.
    #[test]
    fn a_session_token_is_signed_and_not_merely_sent() {
        let signer = Signer {
            access_key_id: ACCESS_KEY.to_string(),
            secret_access_key: SECRET_KEY.to_string(),
            session_token: Some("FQoGZXIvYXdzEA==".to_string()),
            region: "us-east-1".to_string(),
            service: "athena".to_string(),
        };
        let headers = signer.sign(
            &Unsigned {
                method: "POST",
                path: "/",
                query: "",
                headers: vec![(
                    "Host".to_string(),
                    "athena.us-east-1.amazonaws.com".to_string(),
                )],
                payload: b"{}",
            },
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160),
        );
        let authorization = headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.clone())
            .expect("an authorization header");
        assert!(
            authorization.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"),
            "{authorization}"
        );
        assert!(
            headers
                .iter()
                .any(|(name, _)| name == "x-amz-security-token"),
            "the token has to be sent as well as signed"
        );
    }

    /// Lower case, always. Upper-case hex is refused with no explanation.
    #[test]
    fn hex_is_lower_case_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }
}
