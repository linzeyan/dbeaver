//! Key-pair authentication: an RSA private key on disk, and the JWT every
//! request carries.
//!
//! **This is the one file in the driver that is checked rather than read.** No
//! Snowflake account has seen any of it, so what the server does with the token
//! is unknown — but a JWT is not a protocol, it is arithmetic, and every step
//! between the key file and the `Authorization` header can be verified without
//! anybody to send it to. The tests at the bottom do exactly that: the
//! fingerprint is compared against what `openssl dgst -sha256` says about the
//! same key, and the finished token is verified with RS256 the way a server
//! would verify it. What remains unknown is whether Snowflake accepts a token
//! that is arithmetically correct, which is a question only an account answers.
//!
//! The awkward step is the fingerprint, and it is awkward for a reason worth
//! recording. Snowflake identifies the key by `SHA256:` and the base64 of the
//! SHA-256 of the **SubjectPublicKeyInfo** DER — the thing `openssl rsa -pubout
//! -outform DER` writes. `ring` hands back the public key as a PKCS#1
//! `RSAPublicKey`, which is the *inside* of that structure and 24 bytes shorter.
//! Hashing what ring gives would produce a fingerprint that is stable, plausible
//! and rejected by every Snowflake account there is, so the wrapper is built
//! here by hand.

use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::SnowflakeError;

/// How long a minted token claims to be good for.
///
/// Snowflake refuses a JWT whose lifetime exceeds one hour, so this is under it
/// with room for a clock that disagrees with the server's by a few minutes —
/// which is the failure this number exists to survive, since a token rejected
/// for being from the future looks exactly like a wrong key.
const LIFETIME: u64 = 3540;

/// How long before expiry a cached token is replaced.
///
/// A token minted at the last moment can still be in flight when it expires. Ten
/// minutes is longer than any single request has a right to take and short
/// enough that the signature is computed a handful of times an hour rather than
/// once per page.
const RENEW_BEFORE: u64 = 600;

/// The DER `AlgorithmIdentifier` for `rsaEncryption` with its NULL parameters.
///
/// A constant rather than an encoder, because there is exactly one algorithm a
/// Snowflake key pair can be — `1.2.840.113549.1.1.1` — and fifteen bytes that
/// cannot change is clearer than a general one that can be wrong.
const RSA_ENCRYPTION: &[u8] = &[
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];

/// base64url without padding, which is what every part of a JWT is written in.
const URL_SAFE: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// What this session proves itself with.
///
/// Two, and the phase's exit condition is why both are here: *cloud
/// authentication works natively — OAuth, key pair, IAM — without embedded
/// browsers or similar*. The key pair is that condition met in this file. OAuth
/// is a token somebody else obtained, carried; the flows that mint one are a
/// browser redirect or an external identity provider, and neither is something
/// this driver should be pretending to do.
pub(crate) enum Credential {
    /// Boxed because a `Signer` holds a whole RSA key pair and the other variant
    /// is a string: without it every `Credential` anywhere would be the size of
    /// the key.
    KeyPair(Box<Signer>),
    /// An access token from Snowflake's OAuth, as issued. Not refreshed: a
    /// refresh needs the client secret of the integration that issued it, which
    /// is not in a connection string and should not be.
    OAuth(String),
}

impl Credential {
    /// The bearer token and the value Snowflake wants beside it.
    ///
    /// `X-Snowflake-Authorization-Token-Type` is not optional and not
    /// guessable from the token: the SQL API takes both kinds as
    /// `Authorization: Bearer`, and the header is the only thing that says which
    /// this one is.
    pub fn bearer(&self) -> Result<(String, &'static str), SnowflakeError> {
        match self {
            Credential::KeyPair(signer) => Ok((signer.token()?, "KEYPAIR_JWT")),
            Credential::OAuth(token) => Ok((token.clone(), "OAUTH")),
        }
    }
}

/// One RSA key pair, and the tokens it mints.
pub(crate) struct Signer {
    key: RsaKeyPair,
    /// `<ACCOUNT>.<USER>.SHA256:<fingerprint>`.
    issuer: String,
    /// `<ACCOUNT>.<USER>`.
    subject: String,
    random: SystemRandom,
    /// The token last minted and the second it expires. Signing RSA-2048 costs a
    /// millisecond or so, which is nothing once and something on every page of
    /// every partition of every statement.
    cached: Mutex<Option<(String, u64)>>,
}

impl Signer {
    /// Reads a PEM private key and prepares the claims every token repeats.
    ///
    /// `account` is the account identifier as it appears in the host name, and
    /// `user` is the login name. Both are folded to upper case here rather than
    /// wherever they came from, because the claim is about the account and not
    /// about how somebody typed it.
    pub fn new(pem: &str, account: &str, user: &str) -> Result<Self, SnowflakeError> {
        let key = key_of(pem)?;
        let subject = format!("{}.{}", qualified(account), user.to_uppercase());
        let issuer = format!("{subject}.SHA256:{}", fingerprint(key.public().as_ref()));
        Ok(Self {
            key,
            issuer,
            subject,
            random: SystemRandom::new(),
            cached: Mutex::new(None),
        })
    }

    /// A token that is good now, minting one if the last has nearly expired.
    pub fn token(&self) -> Result<String, SnowflakeError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SnowflakeError::Auth("this machine's clock is before 1970".to_string()))?
            .as_secs();
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((token, expiry)) = cached.as_ref()
            && *expiry > now + RENEW_BEFORE
        {
            return Ok(token.clone());
        }
        let token = self.token_at(now)?;
        *cached = Some((token.clone(), now + LIFETIME));
        Ok(token)
    }

    /// The token this key would mint at `now`, in seconds since the epoch.
    ///
    /// Split out from `token` so the tests can pin a moment: a signature over
    /// claims containing the current time is a different signature every run,
    /// and what wants checking is that it verifies and that the claims say what
    /// Snowflake reads.
    fn token_at(&self, now: u64) -> Result<String, SnowflakeError> {
        let header = URL_SAFE.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE.encode(
            serde_json::json!({
                "iss": self.issuer,
                "sub": self.subject,
                "iat": now,
                "exp": now + LIFETIME,
            })
            .to_string(),
        );
        let signed = format!("{header}.{claims}");

        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &self.random,
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(|_| SnowflakeError::Auth("this key could not sign a token".to_string()))?;
        Ok(format!("{signed}.{}", URL_SAFE.encode(&signature)))
    }
}

/// A PEM private key as a key pair, whichever of the two spellings it is in.
///
/// Both, because both are what somebody arrives with. Snowflake's own
/// instructions produce PKCS#8 — `openssl genrsa … | openssl pkcs8` — and the
/// header says `BEGIN PRIVATE KEY`; a key made the older way says `BEGIN RSA
/// PRIVATE KEY` and is PKCS#1. They are different DER structures and `ring` has
/// a constructor for each, so telling them apart is reading the label.
///
/// An encrypted key is refused with a sentence saying so. `ring` has no
/// passphrase support at all, and the alternative — pulling in a PKCS#5 key
/// derivation and a block cipher — is a lot of cryptography to add for a file
/// the user can decrypt in one `openssl pkcs8` command. Refusing loudly is worth
/// more than a `KeyRejected` that says "InvalidEncoding".
fn key_of(pem: &str) -> Result<RsaKeyPair, SnowflakeError> {
    let (label, der) = der_of(pem)?;
    match label.as_str() {
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        "ENCRYPTED PRIVATE KEY" => {
            return Err(SnowflakeError::Auth(
                "this private key is passphrase-encrypted, and this driver cannot decrypt one. \
                 `openssl pkcs8 -in rsa_key.p8 -out rsa_key_decrypted.p8` writes a key it can read"
                    .to_string(),
            ));
        }
        other => {
            return Err(SnowflakeError::Auth(format!(
                "expected an RSA private key and this file holds a {other}"
            )));
        }
    }
    .map_err(|e| SnowflakeError::Auth(format!("this private key could not be read: {e}")))
}

/// The label and the bytes of the first PEM block in `text`.
///
/// Written out rather than taken from a crate, because the whole of PEM is a
/// header line, base64, and a footer line — and the one subtlety, that the
/// base64 is wrapped and a decoder will not take the newlines, is one `retain`.
fn der_of(text: &str) -> Result<(String, Vec<u8>), SnowflakeError> {
    let start = text
        .find("-----BEGIN ")
        .ok_or_else(|| SnowflakeError::Auth("this file is not a PEM private key".to_string()))?;
    let head_end = text[start..]
        .find("-----\n")
        .or_else(|| text[start..].find("-----\r\n"))
        .map(|at| start + at)
        .ok_or_else(|| SnowflakeError::Auth("this PEM file has no complete header".to_string()))?;
    let label = text[start + "-----BEGIN ".len()..head_end].to_string();
    let body_start = head_end + "-----".len();
    let body_end = text[body_start..]
        .find("-----END")
        .map(|at| body_start + at)
        .ok_or_else(|| SnowflakeError::Auth("this PEM file has no end marker".to_string()))?;

    let mut body = text[body_start..body_end].to_string();
    body.retain(|c| !c.is_ascii_whitespace());
    let der = base64::engine::general_purpose::STANDARD
        .decode(&body)
        .map_err(|e| SnowflakeError::Auth(format!("this PEM file did not decode: {e}")))?;
    Ok((label, der))
}

/// The account identifier as the JWT claims spell it.
///
/// Upper case, with the region and cloud dropped. An account reached at
/// `xy12345.us-east-1.aws.snowflakecomputing.com` has the account identifier
/// `xy12345.us-east-1.aws`, and the claim wants `XY12345` — the locator alone.
/// The exception is an account in an organization's global URL, where the part
/// before the first `-` is the identifier and the dot belongs to `.global`
/// itself.
///
/// This is Snowflake's own published rule, transcribed. It is also the single
/// most likely thing in this driver to be wrong in a way that produces
/// `JWT token is invalid` and nothing more specific, which is why it is a
/// function with tests rather than two lines inside `Signer::new`.
fn qualified(account: &str) -> String {
    let account = account.to_uppercase();
    let cut = if account.contains(".GLOBAL") {
        account.find('-')
    } else {
        account.find('.')
    };
    match cut {
        Some(at) => account[..at].to_string(),
        None => account,
    }
}

/// The base64 SHA-256 of the public key, as Snowflake states it.
///
/// Of the SubjectPublicKeyInfo, which is the part worth being exact about; see
/// the module comment.
fn fingerprint(pkcs1: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, &spki(pkcs1));
    base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
}

/// A PKCS#1 `RSAPublicKey` wrapped in the `SubjectPublicKeyInfo` around it.
fn spki(pkcs1: &[u8]) -> Vec<u8> {
    // `BIT STRING`, whose first content byte counts the unused bits at the end —
    // always zero for a whole number of bytes.
    let mut bits = Vec::with_capacity(pkcs1.len() + 6);
    bits.push(0x03);
    der_length(pkcs1.len() + 1, &mut bits);
    bits.push(0x00);
    bits.extend_from_slice(pkcs1);

    let body = RSA_ENCRYPTION.len() + bits.len();
    let mut out = Vec::with_capacity(body + 4);
    out.push(0x30);
    der_length(body, &mut out);
    out.extend_from_slice(RSA_ENCRYPTION);
    out.extend_from_slice(&bits);
    out
}

/// A DER length, in the short form where it fits and the long form where it
/// does not.
///
/// The long form is not an edge case here: an RSA-2048 public key is 270 bytes,
/// so every key Snowflake accepts takes it.
fn der_length(length: usize, out: &mut Vec<u8>) {
    if length < 0x80 {
        out.push(length as u8);
        return;
    }
    let bytes = length.to_be_bytes();
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .expect("a length of zero took the short form");
    out.push(0x80 | (bytes.len() - first) as u8);
    out.extend_from_slice(&bytes[first..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway RSA-2048 key, generated for these tests and used nowhere.
    ///
    /// It has never been registered with a Snowflake account — trivially, since
    /// this driver was written without one — and exists because a signer cannot
    /// be tested without a key to sign with. The expected fingerprint below is
    /// what `openssl rsa -pubout -outform DER | openssl dgst -sha256 -binary |
    /// openssl base64` says about this same key, which is the point: it is a
    /// second implementation of the step this file gets wrong most easily.
    const TEST_KEY: &str = "\
-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC55gkP45dQXp+x
tyIplZtS4i7Q9r2LMgtvkPB7k3X+j6ta4D104BZ5dOj1zKaWLcfWsxF9vensR3Ib
B9fFJ4BVQM1DBmXPA3lrwykHT9H/YfSZCQQ8AZQlfL/A3xcmOdjwYEmfCT8bqCxM
D0HRfPBJ13M/OdEkLGcwwHaazAJXbQidI3lxQoL/E36S0z0X865r9JviKrySCJuc
Q08HNWhuuj4oBHo0R9idloR+y/3LxbdIQfkO8Ln1V4nVDi9H5xg+epZpe+Qg4+gE
LK1H6KGWcjvzKP3qUjwRvKqXyIX7mkPLD1YP+Q0yfHeEc7mWX/cJ/H5dZw3axp+Z
w24MzDFjAgMBAAECggEAWwmWzYOw4eh8/zyGi+ParXPb5nS3LTgkVo4d3v6/jZsR
GQ9wuVBWYUOKJHmE6U3hLDkEa1Y6fP4eGLO2DLSECfwSqWy0JfV3HHl2GcESv6Ta
cqlyO+qwAM2/YDZAcXVp0ons8+fE0ogArXzZSDyNtjO/Gir3y2W9YSgXzUX0LZh+
JBGsTvmVtLYENy7sKtQrSYZx+VKrezrY54zADYtub5XlPU2L9xAcLFA8DEnabcxv
7dGWtXzXGvk2XaQLCB6WDKGAoHVSuY1qntMUdohdTjIfMW4tnAq5nKH5WYq5akHL
qLbS4j8VMB42NOCJcLdw2ZrXsCK89EDwQf0+3mYGfQKBgQD2EDOi7VaPvMY0BSf3
73ew5o9bdlbiOGfimxGQQVjUhBcvFE8DPxbO+f+pL70ugPeP+mtynCtLOieEFFZG
6QEZ72SmuUQih5ulsc99QHfU6H+Z+M+1BrvdwiarXZFdDOai43WI1ZVcrBogOvAU
JbyuzFs57qghPOeiS04DOLN25wKBgQDBZ9oURZIXFC8wTy4yWwxe+s6Sl6cpl8gZ
72vrq4HxDamxVQ23yRPFK3oK+dKFo/eQvyKg02jROtUEOfsjyC22VfLyD8iy4RTD
AwNDS5a4iNR5hFmq0nF9+HRdTCdmFoW7lPLyFSrn4DlAcBw9t12r1ZJbZJmwhKqV
ip73vT2uJQKBgB8+22+695z0+a4tYW/oZqh9/oI8urerNfXefxJ0WdVSmKcPyyC8
aCcMM9zGBR3cnpMX14EMN6srzUzGUFZczBkA/yT0raQ82BToSVK8VvsgMuPYZne0
TTLRrptgHE9WjgrtG0Wu6XKFICQrl8TXLeh8ZrEqjwr5cuh264cZMiDNAoGAZ4Mx
0Q+7NOb0qqJ2UzUv1dXeoc7RBQ3bZyYhWK0eiumJHQQsp2TTVAAE/cLfze8IHUxv
OCxuOS2HvQ9bPrdw39n4gV25SSP2fLksEeRu8q0pKzCO3UJsw8MqZJTRsW30fYUm
0jJKGHiFq9tVAiMV21YfUxLwvu0Cb68VjfqW/JECgYB7fOpsL93dM4mZJA9AXP/r
YDoBRsb34CMdaO8YcGLXsyjljcFSR1r4YRDT6U1asHF6h6WIvpx+MsOVEGRDBBQl
fil6Lex1k1xc2iGyae6VU+zEy6CvVCEmZj1B7pXDzUk3/YAJKrh5R9d60kYc/eO6
Rr2CAulJ6RJ5ieQf18bItQ==
-----END PRIVATE KEY-----
";

    /// What openssl says the fingerprint of `TEST_KEY` is.
    const OPENSSL_FINGERPRINT: &str = "K50KhgARL2yBdl2DVj9UcPZ6G/cuMVt8dQJLX5EL8Jw=";

    fn signer() -> Signer {
        Signer::new(TEST_KEY, "xy12345.us-east-1.aws", "svc_reader").expect("a key")
    }

    /// The step that is 24 bytes away from silently wrong, checked against a
    /// second implementation of it.
    ///
    /// A fingerprint over ring's PKCS#1 bytes rather than the SPKI around them
    /// would still be a base64 SHA-256 of something, would be identical on every
    /// run, and would be refused by every Snowflake account with `JWT token is
    /// invalid`. Only openssl's answer tells the two apart.
    #[test]
    fn the_fingerprint_is_of_the_key_openssl_hashes() {
        let key = key_of(TEST_KEY).expect("a key");
        assert_eq!(fingerprint(key.public().as_ref()), OPENSSL_FINGERPRINT);
    }

    /// The SPKI wrapper is 24 bytes of DER around the key, and every one of them
    /// is a place to be off by one.
    #[test]
    fn the_public_key_wrapper_is_der_of_the_shape_it_claims() {
        let key = key_of(TEST_KEY).expect("a key");
        let pkcs1 = key.public().as_ref();
        let wrapped = spki(pkcs1);

        assert_eq!(wrapped.len(), pkcs1.len() + 24);
        // SEQUENCE, long-form length of two bytes.
        assert_eq!(wrapped[0], 0x30);
        assert_eq!(wrapped[1], 0x82);
        assert_eq!(
            usize::from(u16::from_be_bytes([wrapped[2], wrapped[3]])),
            wrapped.len() - 4,
            "the outer length has to cover everything after it"
        );
        assert_eq!(&wrapped[4..4 + RSA_ENCRYPTION.len()], RSA_ENCRYPTION);
        // BIT STRING, its own long-form length of two bytes, the count of unused
        // bits, and then the key itself.
        let bits = 4 + RSA_ENCRYPTION.len();
        assert_eq!(&wrapped[bits..bits + 2], &[0x03, 0x82]);
        assert_eq!(
            usize::from(u16::from_be_bytes([wrapped[bits + 2], wrapped[bits + 3]])),
            pkcs1.len() + 1,
            "the bit string covers the key and the unused-bit count"
        );
        assert_eq!(wrapped[bits + 4], 0x00);
        assert_eq!(&wrapped[bits + 5..], pkcs1);
    }

    /// Short form under 128 bytes, long form at and above it. An RSA key always
    /// takes the second, so the first is only ever exercised by a mistake.
    #[test]
    fn a_der_length_takes_the_form_its_size_requires() {
        let encode = |n| {
            let mut out = Vec::new();
            der_length(n, &mut out);
            out
        };
        assert_eq!(encode(0), vec![0x00]);
        assert_eq!(encode(127), vec![0x7f]);
        assert_eq!(encode(128), vec![0x81, 0x80]);
        assert_eq!(encode(270), vec![0x82, 0x01, 0x0e]);
        assert_eq!(encode(65535), vec![0x82, 0xff, 0xff]);
        assert_eq!(encode(65536), vec![0x83, 0x01, 0x00, 0x00]);
    }

    /// A token a server would accept as a signature, verified the way a server
    /// verifies one — over the first two parts, with the public key, as RS256.
    ///
    /// The claims are read back rather than trusted, because a token that
    /// verifies and claims the wrong issuer is exactly the failure this driver
    /// cannot debug from here.
    #[test]
    fn a_minted_token_verifies_as_rs256_and_claims_what_snowflake_reads() {
        let signer = signer();
        let token = signer.token_at(1_700_000_000).expect("a token");

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT has three parts");

        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE.decode(parts[0]).expect("base64url")).expect("json");
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");

        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE.decode(parts[1]).expect("base64url")).expect("json");
        assert_eq!(
            claims["iss"],
            format!("XY12345.SVC_READER.SHA256:{OPENSSL_FINGERPRINT}")
        );
        assert_eq!(claims["sub"], "XY12345.SVC_READER");
        assert_eq!(claims["iat"], 1_700_000_000u64);
        assert_eq!(claims["exp"], 1_700_000_000u64 + LIFETIME);

        let key = key_of(TEST_KEY).expect("a key");
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            key.public().as_ref(),
        );
        public
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &URL_SAFE.decode(parts[2]).expect("base64url"),
            )
            .expect("the signature should verify against its own public key");
    }

    /// base64url and not base64: a `+` or a `/` in the signature would be a
    /// token the server splits in the wrong place, and padding would be a token
    /// it refuses to parse. Both are invisible until a key happens to produce
    /// one, which is most of them.
    #[test]
    fn every_part_of_the_token_is_url_safe_and_unpadded() {
        let token = signer().token_at(1_700_000_000).expect("a token");
        assert!(
            !token.contains('+') && !token.contains('/') && !token.contains('='),
            "got: {token}"
        );
    }

    /// The rule that decides which account the token is for, on the three shapes
    /// of account identifier Snowflake publishes.
    #[test]
    fn an_account_identifier_loses_its_region_and_keeps_its_organization() {
        // An account locator with a region and a cloud behind it.
        assert_eq!(qualified("xy12345.us-east-1.aws"), "XY12345");
        assert_eq!(qualified("xy12345.eu-central-1"), "XY12345");
        // A locator on AWS US West, which has no region in its identifier.
        assert_eq!(qualified("xy12345"), "XY12345");
        // The organization form, which keeps its hyphen.
        assert_eq!(qualified("myorg-my_account"), "MYORG-MY_ACCOUNT");
        // A global URL, where the dot belongs to `.global` and the identifier
        // ends at the hyphen instead.
        assert_eq!(qualified("myorg-my_account.global"), "MYORG");
        // Private link, which is a suffix on the locator like a region is.
        assert_eq!(qualified("xy12345.privatelink"), "XY12345");
    }

    /// The older PEM spelling is a different DER structure, not a different
    /// header — a driver that took the label as decoration would hand PKCS#1
    /// bytes to a PKCS#8 parser and report "this private key could not be read"
    /// about a perfectly good key.
    #[test]
    fn both_pem_spellings_read_as_the_same_key() {
        let (label, der) = der_of(TEST_KEY).expect("a block");
        assert_eq!(label, "PRIVATE KEY");
        assert_eq!(der.len(), 1216, "the PKCS#8 DER of this RSA-2048 key");

        let pkcs8 = key_of(TEST_KEY).expect("a key");
        // The same key, written the other way. PKCS#8 is the PKCS#1 structure
        // inside an OCTET STRING, behind a version and an algorithm identifier —
        // 26 bytes for an RSA-2048 key — so the rest of the file is exactly what
        // an `RSA PRIVATE KEY` block holds. Both must produce the same public
        // key, or one of the two constructors is being handed the wrong thing.
        let inner = &der[26..];
        let traditional = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(inner)
        );
        let pkcs1 = key_of(&traditional).expect("the same key, written the older way");
        assert_eq!(pkcs8.public().as_ref(), pkcs1.public().as_ref());
    }

    /// An encrypted key is the one failure a user can act on, so it says what to
    /// do rather than what went wrong.
    #[test]
    fn an_encrypted_key_says_how_to_decrypt_it() {
        let encrypted =
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n";
        let message = key_of(encrypted).expect_err("a refusal").to_string();
        assert!(message.contains("openssl pkcs8"), "got: {message}");
    }

    #[test]
    fn something_that_is_not_a_key_is_refused_before_anything_is_signed() {
        assert!(key_of("hunter2").is_err());
        assert!(
            key_of("-----BEGIN PRIVATE KEY-----\nnot base64 at all!\n-----END PRIVATE KEY-----\n")
                .is_err()
        );
    }

    /// An OAuth token is carried as it arrived and says it is one, because the
    /// header is the only thing that tells the server which kind it is holding.
    #[test]
    fn an_oauth_token_is_carried_rather_than_minted() {
        let (token, kind) = Credential::OAuth("ver:1-hint:abc".to_string())
            .bearer()
            .expect("a token");
        assert_eq!(token, "ver:1-hint:abc");
        assert_eq!(kind, "OAUTH");
    }
}
