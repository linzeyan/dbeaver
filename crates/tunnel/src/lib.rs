//! A local port that forwards to a database through an SSH server.
//!
//! Drivers cannot do this for themselves: every one of them takes an address
//! and dials it, so reaching a database behind a bastion means having a local
//! address to give them. `Tunnel::open` returns one, and holds the forward open
//! for as long as the value lives.
//!
//! The host-key record is the user's own `~/.ssh/known_hosts`, which is a
//! decision rather than a default. A password is about to be sent to whatever
//! answers, so something has to vouch for the server first — and the file that
//! already knows every bastion this person uses is a better answer than a
//! private list this application would have to teach them to fill in.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use russh::client;
use russh::keys::PublicKey;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Why a tunnel could not be opened.
///
/// The two host-key cases are separate, and both say that nothing was sent,
/// because they call for opposite things from the person reading them: an
/// unknown host is ordinary and is fixed by recording it, while a changed key
/// is either a rebuilt server or somebody in the middle and must not be waved
/// through with a retry.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error(
        "the server at {host}:{port} is not in {file}. Connect once with ssh, or add it with \
         ssh-keyscan, so its identity is on record before a password is sent to it"
    )]
    UnknownHost {
        host: String,
        port: u16,
        file: String,
    },
    #[error(
        "the key {host}:{port} presented is not the one recorded in {file}. Either the server was \
         rebuilt or something is answering in its place; nothing was sent to it"
    )]
    HostKeyChanged {
        host: String,
        port: u16,
        file: String,
    },
    #[error("the server refused the password for {0}")]
    Rejected(String),
    #[error("{0}")]
    Ssh(#[from] russh::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Where the bastion is and who to log in as.
///
/// Everything about the tunnel except what it forwards to, which is passed to
/// `Tunnel::open` instead: a connection string already names the host and port
/// the database is on, and a field here holding a second copy of that would be
/// a field every caller had to remember to keep in step.
pub struct TunnelConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub known_hosts: PathBuf,
}

/// Decides whether the server that answered is the one expected.
///
/// `check_known_hosts_path` takes the file to read rather than finding it, so
/// this is testable against a fixture instead of against whatever is in the
/// home directory of whoever is running the tests.
struct Gatekeeper {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

impl client::Handler for Gatekeeper {
    type Error = TunnelError;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let file = self.known_hosts.display().to_string();
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            // Returning the error rather than `Ok(false)`: refusing the key
            // aborts the handshake with russh's own words, and those words do
            // not say which of the two things went wrong or what to do next.
            Ok(false) => Err(TunnelError::UnknownHost {
                host: self.host.clone(),
                port: self.port,
                file,
            }),
            Err(_) => Err(TunnelError::HostKeyChanged {
                host: self.host.clone(),
                port: self.port,
                file,
            }),
        }
    }
}

/// A local forward, open for as long as this value lives.
pub struct Tunnel {
    local: SocketAddr,
    accepting: JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // The accept loop owns the SSH session, so aborting it is what closes
        // the connection to the bastion. Without this a closed connection in
        // the application would leave a task holding a socket open until the
        // process ended.
        self.accepting.abort();
    }
}

impl Tunnel {
    /// The address to hand a driver instead of the database's own.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Connects, authenticates, and starts forwarding.
    ///
    /// Bound to port 0 and the loopback address: the kernel picks a free port,
    /// which avoids both a collision with whatever else is running and the
    /// question of what to do when the port someone configured is taken.
    /// Loopback because a forward reachable from the network would put the
    /// database in front of anyone who can reach this machine.
    /// `target_host` is resolved by the SSH server, not here. That is the whole
    /// point of a bastion: the name is one only the far side can look up.
    pub async fn open(
        config: TunnelConfig,
        target_host: &str,
        target_port: u16,
    ) -> Result<Tunnel, TunnelError> {
        let gate = Gatekeeper {
            host: config.host.clone(),
            port: config.port,
            known_hosts: config.known_hosts.clone(),
        };
        let mut session = client::connect(
            Arc::new(client::Config::default()),
            (config.host.as_str(), config.port),
            gate,
        )
        .await?;
        if !session
            .authenticate_password(&config.user, &config.password)
            .await?
            .success()
        {
            return Err(TunnelError::Rejected(config.user));
        }

        let listener = TcpListener::bind(("127.0.0.1", 0u16)).await?;
        let local = listener.local_addr()?;
        let target_host = target_host.to_owned();
        let target_port = u32::from(target_port);
        let accepting = tokio::spawn(async move {
            while let Ok((mut inbound, peer)) = listener.accept().await {
                let opened = session
                    .channel_open_direct_tcpip(
                        target_host.clone(),
                        target_port,
                        peer.ip().to_string(),
                        u32::from(peer.port()),
                    )
                    .await;
                // A refused channel is the far side declining this one
                // connection — the database is down, or the server forbids
                // forwarding. The listener stays up, because the alternative is
                // a tunnel that silently stops existing after one bad dial.
                let Ok(channel) = opened else { continue };
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut stream).await;
                });
            }
        });
        Ok(Tunnel { local, accepting })
    }
}
