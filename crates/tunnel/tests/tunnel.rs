//! A tunnel to a database this machine cannot reach any other way.
//!
//! `make db-up-ssh` puts up an sshd and writes down its host keys; `make db-up`
//! puts PostgreSQL on the same compose network under the name `pg`. That name
//! resolves inside the network and nowhere else, and that is the point: a
//! forward ending at a port this process could have dialled directly would
//! prove that a socket opened, not that anything crossed the bastion.

use std::path::PathBuf;

use dbtunnel::{Tunnel, TunnelConfig, TunnelError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Where `make db-up-ssh` wrote the fixture's host keys.
///
/// Read from the environment first, with a compile-time fallback, for the same
/// reason `PGTLS_CA` is: `CARGO_MANIFEST_DIR` is baked into the test binary
/// when it is built, so a workspace sharing one `target/` between git worktrees
/// hands back a cached binary naming the worktree it was compiled in — which
/// may since have been removed.
fn known_hosts() -> PathBuf {
    std::env::var("SSH_KNOWN_HOSTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/ssh/known_hosts"
            ))
        })
}

/// The bastion these tests log in to.
fn config() -> TunnelConfig {
    TunnelConfig {
        host: "127.0.0.1".into(),
        port: 52222,
        user: "bench".into(),
        password: "bench".into(),
        known_hosts: known_hosts(),
    }
}

/// What the tunnel forwards to: the compose service name, which resolves inside
/// the compose network and nowhere else.
const TARGET: (&str, u16) = ("pg", 5432);

#[tokio::test]
#[ignore = "requires the SSH server and the benchmark database (make db-up-ssh db-up)"]
async fn postgres_answers_through_the_forward() {
    let tunnel = Tunnel::open(config(), TARGET.0, TARGET.1)
        .await
        .expect("the tunnel opened");
    let mut probe = tokio::net::TcpStream::connect(tunnel.local_addr())
        .await
        .expect("the local end of the tunnel accepted a connection");

    // An SSLRequest: the eight bytes every PostgreSQL client opens with, and
    // the only message the server answers before it has been told anything.
    // Asked at this level rather than with the driver because what is under
    // test is the forward — a driver here would drag the whole connection path
    // in to prove one thing about this one.
    probe
        .write_all(&[0, 0, 0, 8, 4, 210, 22, 47])
        .await
        .expect("the request crossed the tunnel");
    let mut answer = [0u8; 1];
    probe
        .read_exact(&mut answer)
        .await
        .expect("and an answer came back");

    assert!(
        matches!(answer[0], b'N' | b'S'),
        "expected PostgreSQL's yes or no to TLS, got {:?}",
        answer[0] as char
    );
}

/// The half that matters if any of this is to be worth having.
///
/// A password is about to be sent to whatever answered, so a server nobody has
/// vouched for has to stop the exchange before that happens — and it has to say
/// which of the two host-key failures it was, because recording an unknown host
/// is routine and accepting a changed key is not.
#[tokio::test]
#[ignore = "requires the SSH server (make db-up-ssh)"]
async fn a_server_that_is_not_on_record_is_not_given_the_password() {
    let empty = std::env::temp_dir().join("dbtunnel-empty-known-hosts");
    std::fs::write(&empty, b"").expect("an empty known_hosts file");

    // `let Err(...) else` rather than `expect_err`, which wants the success type
    // to be Debug — and a forward is a live socket and a running task, so
    // deriving Debug on it to satisfy a test would be the test deciding the
    // public API.
    let Err(error) = Tunnel::open(
        TunnelConfig {
            known_hosts: empty.clone(),
            ..config()
        },
        TARGET.0,
        TARGET.1,
    )
    .await
    else {
        panic!("a server that is not on record must not be given the password")
    };

    let _ = std::fs::remove_file(&empty);
    assert!(
        matches!(error, TunnelError::UnknownHost { .. }),
        "expected the unknown-host refusal, got {error}"
    );
}

#[tokio::test]
#[ignore = "requires the SSH server (make db-up-ssh)"]
async fn a_refused_password_says_so() {
    let Err(error) = Tunnel::open(
        TunnelConfig {
            password: "not the password".into(),
            ..config()
        },
        TARGET.0,
        TARGET.1,
    )
    .await
    else {
        panic!("the wrong password must not open a tunnel")
    };

    assert!(
        matches!(error, TunnelError::Rejected(_)),
        "expected the refusal to name itself, got {error}"
    );
}
