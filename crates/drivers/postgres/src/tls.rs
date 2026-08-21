//! What `sslmode` asks for, and what tokio-postgres has to be told instead.
//!
//! This driver had `NoTls` at six call sites, two of them cancels. A cancel
//! sent in the clear to a server that requires TLS is refused, and the statement
//! it was meant to stop keeps running — so the decision cannot live at the place
//! that opens the session. It lives in [`Tls`], which every socket in this crate
//! now goes through.
//!
//! The five mode names are libpq's, because they are what people have in their
//! notes and in the URLs they paste. tokio-postgres knows only three of them and
//! refuses to parse a string containing either of the other two, and it has
//! never heard of `sslrootcert` at all; so both come out of the string here,
//! before it is handed over.

use std::sync::Arc;

use percent_encoding::percent_decode_str;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};
use tokio_postgres::{CancelToken, Client, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::PgError;

/// How much of the server's identity has to be proved before anything is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    /// Never encrypt.
    Disable,
    /// Encrypt if the server offers it, and carry on in the clear if it does
    /// not.
    ///
    /// The default, because it is libpq's: a client quieter about TLS than
    /// `psql` is one that downgrades a connection without saying so, and every
    /// URL anybody pastes here was written for a client that behaves this way.
    /// It proves nothing about who answered — see [`SslMode::Require`], whose
    /// certificate handling this shares.
    #[default]
    Prefer,
    /// Encrypt, and accept any certificate.
    ///
    /// Proof against somebody reading the wire, and none at all about who is on
    /// the other end of it. That is what libpq's `require` means, and being
    /// stricter than the name under the same name would be this driver deciding
    /// the word meant something else.
    Require,
    /// Encrypt, and check that the certificate chains to a trusted root —
    /// without checking the name on it.
    ///
    /// The mode for a server reached by address or through a tunnel, where the
    /// name will never match and the chain is the whole of what can be proved.
    VerifyCa,
    /// Encrypt, check the chain, and check the name.
    VerifyFull,
}

impl SslMode {
    /// The mode libpq spells `word`, or `None` for a word libpq does not have.
    fn named(word: &str) -> Option<Self> {
        Some(match word {
            "disable" => SslMode::Disable,
            // libpq's `allow` prefers the clear and falls back to TLS, the
            // mirror of `prefer`. The wire protocol tokio-postgres speaks has no
            // way to ask for that order, so it is read as the neighbour it is
            // one step from rather than refused: both end up encrypted where
            // encryption is available, which is what somebody who wrote either
            // word was after.
            "allow" | "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" => SslMode::VerifyFull,
            _ => return None,
        })
    }

    /// What tokio-postgres has to be told, which is only ever three of the five.
    ///
    /// Which certificate to accept is decided on this side and never on the
    /// wire, so all three modes that encrypt ask the server for the same thing.
    fn wire_word(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => "require",
        }
    }
}

/// What `conn_str` asks for about the wire, and the string with the asking
/// removed.
///
/// tokio-postgres refuses a connection string it cannot parse whole, and it can
/// parse neither `verify-ca`, `verify-full` nor `sslrootcert`. Left in, a URL
/// somebody copied out of their notes fails with a parse error naming the very
/// option this module implements.
///
/// Only the query half is touched, and a string with no `?` is returned exactly
/// as it arrived — which is also what lets the `host=… dbname=…` spelling
/// through untouched rather than through a URL parser that would reject it.
pub fn split_ssl(conn_str: &str) -> Result<(String, SslMode, Option<String>), PgError> {
    let Some((base, query)) = conn_str.split_once('?') else {
        return Ok((conn_str.to_string(), SslMode::default(), None));
    };

    let mut mode = SslMode::default();
    let mut root_cert = None;
    let mut kept: Vec<&str> = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "sslmode" => {
                let word = decode(value);
                mode = SslMode::named(&word).ok_or(PgError::UnknownSslMode(word))?;
            }
            // Percent-decoded because it is a path, and a path can hold a space.
            "sslrootcert" => root_cert = Some(decode(value)),
            _ => kept.push(pair),
        }
    }

    // Written out even where it was not asked for, so that the string handed on
    // says what was decided rather than leaving it to whichever default
    // tokio-postgres happens to hold.
    let wire = format!("sslmode={}", mode.wire_word());
    kept.push(&wire);
    Ok((format!("{base}?{}", kept.join("&")), mode, root_cert))
}

fn decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

/// One decision about the wire, made once and used everywhere a socket opens.
///
/// Including cancel, which opens one of its own. A cancel that went out in the
/// clear against a server requiring TLS would be refused, and the statement it
/// was meant to stop would keep running while the window reported it cancelled
/// — the one outcome worth restructuring this crate to make unreachable.
#[derive(Clone)]
pub enum Tls {
    Off,
    On(MakeRustlsConnect),
}

impl Tls {
    /// What `mode` asks for, as something that can open a socket.
    pub fn new(mode: SslMode, root_cert: Option<&str>) -> Result<Self, PgError> {
        if mode == SslMode::Disable {
            return Ok(Tls::Off);
        }

        // Named rather than taken from the process-wide default. rustls panics
        // at connect time when more than one provider is compiled in and none
        // has been installed, and which providers are compiled in is decided by
        // feature unification across every crate in this workspace — a fact no
        // reader of this file could check and no test here would notice.
        let provider = Arc::new(ring::default_provider());
        let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| PgError::Tls(e.to_string()))?;

        let config = match mode {
            SslMode::Disable => unreachable!("answered above"),
            SslMode::Prefer | SslMode::Require => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
                .with_no_client_auth(),
            SslMode::VerifyCa => {
                let inner = WebPkiServerVerifier::builder_with_provider(
                    Arc::new(roots(root_cert)?),
                    provider,
                )
                .build()
                .map_err(|e| PgError::Tls(e.to_string()))?;
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(AnyName(inner)))
                    .with_no_client_auth()
            }
            SslMode::VerifyFull => builder
                .with_root_certificates(roots(root_cert)?)
                .with_no_client_auth(),
        };
        Ok(Tls::On(MakeRustlsConnect::new(config)))
    }

    /// Opens a connection and spawns the task that drives its socket.
    ///
    /// The task is spawned here rather than handed back, because the connection
    /// future's type differs between the two arms below and a caller that had to
    /// name it would need this enum's shape spelled out at every call site.
    /// `what` names the connection in the line printed if the socket closes
    /// under it: a cursor dying is a different report from a session dying.
    pub async fn connect(&self, conn_str: &str, what: &'static str) -> Result<Client, PgError> {
        match self {
            Tls::Off => {
                let (client, connection) = tokio_postgres::connect(conn_str, NoTls).await?;
                spawn_driver(connection, what);
                Ok(client)
            }
            Tls::On(connector) => {
                let (client, connection) =
                    tokio_postgres::connect(conn_str, connector.clone()).await?;
                spawn_driver(connection, what);
                Ok(client)
            }
        }
    }

    /// Asks the server to stop what `token`'s backend is running.
    ///
    /// On a connection of its own, which is why it needs to be told about TLS at
    /// all: this is the request that used to go out in the clear no matter what
    /// the session had negotiated.
    pub async fn cancel(&self, token: &CancelToken) -> Result<(), PgError> {
        match self {
            Tls::Off => token.cancel_query(NoTls).await?,
            Tls::On(connector) => token.cancel_query(connector.clone()).await?,
        }
        Ok(())
    }
}

/// The connection future drives the socket and must outlive us. There is no
/// reconnect story; a dropped connection surfaces as a query error.
fn spawn_driver<F, E>(connection: F, what: &'static str)
where
    F: std::future::Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("postgres {what} closed: {e}");
        }
    });
}

/// The trusted roots: the Mozilla bundle, plus whatever `sslrootcert` names.
///
/// Added to rather than replacing, so that naming a private CA does not stop a
/// public one from being trusted — a connection string is edited one field at a
/// time, and losing the public roots to add a private one is not something
/// anybody asked for by typing a path.
///
/// The platform store is deliberately not read. On this Mac that would be the
/// Keychain, on Windows it would be something else, and a client whose trust
/// depended on which machine it ran from is one whose failures cannot be
/// reproduced. A private CA is named by path, which is also what libpq does.
fn roots(root_cert: Option<&str>) -> Result<RootCertStore, PgError> {
    let mut store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let Some(path) = root_cert else {
        return Ok(store);
    };
    let bad = |e: &dyn std::fmt::Display| PgError::RootCertificate {
        path: path.to_string(),
        reason: e.to_string(),
    };
    for certificate in CertificateDer::pem_file_iter(path).map_err(|e| bad(&e))? {
        store
            .add(certificate.map_err(|e| bad(&e))?)
            .map_err(|e| bad(&e))?;
    }
    Ok(store)
}

/// Accepts every certificate, which is what `require` asks for.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    // The handshake signature is still checked. It proves the certificate shown
    // belongs to whoever is answering, which is worth having even where nothing
    // proves who that is: without it the encryption could be terminated by
    // anything on the path holding any certificate at all.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Verifies the chain and ignores the name, which is what `verify-ca` asks for.
///
/// Everything is delegated to the ordinary verifier and exactly one of its
/// refusals is swallowed. Written this way round on purpose: a hand-written
/// chain check that happened to skip the name would be a second implementation
/// of the part that matters, and the part that matters is not the part being
/// relaxed.
#[derive(Debug)]
struct AnyName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for AnyName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            // The one refusal `verify-ca` is defined to allow. A server reached
            // by address, or through a tunnel, presents a certificate naming
            // something else and chaining to the right root anyway.
            Err(TlsError::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::{SslMode, split_ssl};

    /// A string that asks nothing is handed on as it arrived — which is also
    /// what lets the `host=… dbname=…` spelling through, since it has no `?`.
    #[test]
    fn a_string_with_no_query_is_left_alone() {
        let (rewritten, mode, ca) = split_ssl("postgres://ana@db.example/sales").unwrap();
        assert_eq!(rewritten, "postgres://ana@db.example/sales");
        assert_eq!(mode, SslMode::Prefer);
        assert_eq!(ca, None);
    }

    /// The two modes tokio-postgres cannot parse come out, and what goes on is
    /// the word it can. This is the case that fails to connect at all without
    /// this function.
    #[test]
    fn the_modes_tokio_postgres_refuses_are_taken_out_of_the_string() {
        let (rewritten, mode, ca) =
            split_ssl("postgres://ana@db.example/sales?sslmode=verify-full").unwrap();
        assert_eq!(rewritten, "postgres://ana@db.example/sales?sslmode=require");
        assert_eq!(mode, SslMode::VerifyFull);
        assert_eq!(ca, None);

        let (rewritten, mode, ca) =
            split_ssl("postgres://h/d?sslmode=verify-ca&sslrootcert=%2Fetc%2Fca%20root.pem")
                .unwrap();
        assert_eq!(rewritten, "postgres://h/d?sslmode=require");
        assert_eq!(mode, SslMode::VerifyCa);
        assert_eq!(ca.as_deref(), Some("/etc/ca root.pem"));
    }

    /// Everything else in the query belongs to tokio-postgres and has to arrive
    /// there. Dropping an unknown parameter would silently lose a setting.
    #[test]
    fn the_parameters_this_module_does_not_own_are_passed_through() {
        let (rewritten, mode, _) =
            split_ssl("postgres://h/d?application_name=dbclient&sslmode=disable&connect_timeout=5")
                .unwrap();
        assert_eq!(
            rewritten,
            "postgres://h/d?application_name=dbclient&connect_timeout=5&sslmode=disable"
        );
        assert_eq!(mode, SslMode::Disable);
    }

    /// `allow` is read as its mirror rather than refused, and a word libpq does
    /// not have is refused rather than quietly becoming the default — a typo in
    /// `verify-full` must not connect with no verification at all.
    #[test]
    fn a_word_libpq_does_not_have_is_refused() {
        let (_, mode, _) = split_ssl("postgres://h/d?sslmode=allow").unwrap();
        assert_eq!(mode, SslMode::Prefer);
        assert!(split_ssl("postgres://h/d?sslmode=verify_full").is_err());
        assert!(split_ssl("postgres://h/d?sslmode=on").is_err());
    }
}
