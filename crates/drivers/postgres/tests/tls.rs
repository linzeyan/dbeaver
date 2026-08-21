//! What each `sslmode` does against a server that insists on TLS.
//!
//! `make db-up-pgtls` puts up a PostgreSQL whose `pg_hba.conf` holds only
//! `hostssl` lines and whose certificate names `pg.internal` — a name nothing
//! here connects to. Both halves matter: without the first, "it connected" says
//! nothing about whether TLS happened, and without the second there is no
//! observable difference between `verify-ca` and `verify-full`.
//!
//! These are the tests the unit tests in `src/tls.rs` cannot be. That module
//! can check which words come out of a connection string; only a server can
//! check that the certificate decision made from them is the one that reaches
//! the wire.

use driver_postgres::PgSource;

/// The address, which is not the name on the certificate.
const SERVER: &str = "postgres://bench:bench@127.0.0.1:55434/bench";

/// The CA that signed the fixture's certificate, which is what `sslrootcert`
/// wants: the root to trust, not the leaf to expect.
///
/// `PGTLS_CA` is read first, and the Makefile sets it, so the fixture's location
/// has one source. The fallback is a compile-time path and that is the whole
/// reason the variable exists: `CARGO_MANIFEST_DIR` is baked into the binary
/// when it is built, so a workspace sharing one `target/` between git worktrees
/// hands back a cached test binary naming the worktree it was compiled in —
/// which may since have been removed. The failure that produces is a missing CA
/// file, reported against a path nobody recognises.
fn certificate() -> String {
    std::env::var("PGTLS_CA").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../../target/pgtls/ca.crt").to_owned()
    })
}

/// Needs no server. The CA file is read while the connection is still being
/// decided, which is why naming one that is not there fails by saying so rather
/// than as a handshake that mysteriously does not complete.
#[tokio::test]
async fn a_ca_file_that_is_not_there_names_itself() {
    let Err(error) = PgSource::connect(&format!(
        "{SERVER}?sslmode=verify-ca&sslrootcert=/no/such/ca.pem"
    ))
    .await
    else {
        panic!("there is no such file, so this must not have connected");
    };
    let said = format!("{error}");
    assert!(said.contains("/no/such/ca.pem"), "got: {said}");
}

/// Needs no server either: the word is refused before anything is opened.
///
/// A typo in `verify-full` must not connect. Read as a default it would connect
/// with no verification at all, which is the one outcome somebody typing that
/// word is trying to avoid.
#[tokio::test]
async fn a_mode_this_build_does_not_have_is_refused_before_connecting() {
    let Err(error) = PgSource::connect(&format!("{SERVER}?sslmode=verify_full")).await else {
        panic!("libpq spells it with a hyphen, so this must not have connected");
    };
    let said = format!("{error}");
    assert!(said.contains("verify_full"), "got: {said}");
}

/// The fixture is a fixture. If this ever passes, every test below it has been
/// proving nothing.
#[tokio::test]
#[ignore = "requires the TLS test server (make db-up-pgtls)"]
async fn the_server_refuses_a_connection_that_asks_for_no_encryption() {
    let Err(error) = PgSource::connect(&format!("{SERVER}?sslmode=disable")).await else {
        panic!("the fixture's pg_hba.conf has no plaintext line, so this must not have connected");
    };
    let said = format!("{error}");
    assert!(said.contains("no encryption"), "got: {said}");
}

/// `require` encrypts and proves nothing, which is what libpq's `require`
/// means: a self-signed certificate from an unknown issuer is exactly the case
/// it accepts.
#[tokio::test]
#[ignore = "requires the TLS test server (make db-up-pgtls)"]
async fn require_encrypts_without_proving_who_answered() {
    PgSource::connect(&format!("{SERVER}?sslmode=require"))
        .await
        .expect("require accepts any certificate, including this one");
}

/// `verify-ca` wants the chain and does not want the name.
///
/// Both directions in one test, because either alone is passed by a mistake:
/// accepting without the CA file would mean nothing is being verified, and
/// refusing with it would mean the name is still being checked.
#[tokio::test]
#[ignore = "requires the TLS test server (make db-up-pgtls)"]
async fn verify_ca_wants_the_chain_and_not_the_name() {
    assert!(
        PgSource::connect(&format!("{SERVER}?sslmode=verify-ca"))
            .await
            .is_err(),
        "a self-signed certificate is in no public root store"
    );

    let certificate = certificate();
    PgSource::connect(&format!(
        "{SERVER}?sslmode=verify-ca&sslrootcert={certificate}"
    ))
    .await
    .expect("named as a root, the chain verifies and the wrong name is allowed");
}

/// `verify-full` refuses exactly what `verify-ca` above allowed, with the same
/// certificate and the same root — the name is the only difference between them.
#[tokio::test]
#[ignore = "requires the TLS test server (make db-up-pgtls)"]
async fn verify_full_refuses_the_name_verify_ca_allows() {
    let certificate = certificate();
    let Err(error) = PgSource::connect(&format!(
        "{SERVER}?sslmode=verify-full&sslrootcert={certificate}"
    ))
    .await
    else {
        panic!("the certificate names pg.internal and this connects to 127.0.0.1");
    };
    let said = format!("{error}").to_lowercase();
    assert!(said.contains("name"), "got: {said}");
}

/// A cancel opens a connection of its own, and used to open it in the clear.
///
/// Against this server that request is refused by `pg_hba.conf` before it can
/// name a backend, so the statement it was meant to stop keeps running while
/// the caller is told it was cancelled. Nothing else in the suite reaches this
/// path with TLS on.
#[tokio::test]
#[ignore = "requires the TLS test server (make db-up-pgtls)"]
async fn a_cancel_goes_out_the_way_the_session_came_in() {
    let source = PgSource::connect(&format!("{SERVER}?sslmode=require"))
        .await
        .expect("require accepts any certificate");
    // Nothing is running, so this cancels nothing and reports it. What is being
    // exercised is the round trip: on a plaintext cancel the server closes the
    // socket and this is an error instead.
    let reached = source
        .cancel()
        .await
        .expect("the cancel request was accepted");
    assert_eq!(reached, 0, "no statement was running to be stopped");
}
