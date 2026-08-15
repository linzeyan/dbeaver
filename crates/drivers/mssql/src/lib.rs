//! SQL Server, behind the same `Driver` trait as the others.
//!
//! Three things about this database, and about the one Rust client for it, shape
//! everything below. They are stated here because none of them is guessable from
//! the code that deals with them.
//!
//! **A connection names one database, and a database has schemas inside it.**
//! SQL Server has three naming levels where PostgreSQL has two, and the trait
//! has one `schema` string. `schemas()` therefore answers with the schemas of
//! the single database the connection string named, and every other metadata
//! call is read in that same database. Reaching a second one means opening a
//! second connection; `databases()` is here so a front end can offer that rather
//! than pretend the instance has only one. The alternatives were both worse:
//! a composite `"MyDb.dbo"` name has to be split back apart by the driver and
//! needs a delimiter that database and schema names can legally contain, and the
//! catalog qualifier in `[MyDb].sys.schemas` is a syntactic element that cannot
//! be bound as a parameter — so nine catalog queries would become dynamic SQL
//! with a user-typed identifier pasted into them. `USE <db>` per call is worse
//! still: it mutates the connection for whoever borrows it next.
//!
//! **Four SQL Server types crash the decoder, so a statement is inspected before
//! it is sent.** `geometry`, `geography` and `hierarchyid` travel as TDS type
//! `Udt` and `sql_variant` as `SSVariant`, and tiberius' `TypeInfo::decode` ends
//! in `todo!()` for both. That panic happens while parsing `COLMETADATA`, before
//! any row, and this workspace builds release with `panic = "abort"` — so an
//! ordinary `SELECT *` over a table with a `geography` column would take the
//! whole application down with no message. Every statement is therefore
//! described by the server first, and one naming those types is refused. See
//! `describe_statement` for what that covers and what it does not.
//!
//! **Cancelling ends the session.** TDS has an Attention packet that stops one
//! statement and leaves the connection usable, and tiberius declares the packet
//! type and never constructs it. Dropping a tiberius future is not a substitute:
//! it is a known way to corrupt the connection (upstream issues #79 and #300).
//! What is left is `KILL <spid>` from a second connection, which ends the
//! session rather than the statement — so after a cancel the connection is gone
//! and, for a cursor, the browse is over rather than paused. For a statement it
//! costs more than it does anywhere else here: statements share one connection
//! so that a transaction can span them, so a cancel takes the open transaction
//! with it and the next statement starts on a connection that has none. The
//! server rolled that transaction back before this side found out, so nothing is
//! lost by admitting it. That is also why a cancel is aimed only at a session
//! that is actually running something: see `Inflight`.

mod arrow_map;
mod driver;
mod metadata;

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use arrow_map::{ColBuilder, ColumnLayout, arrow_field};
use dbconn::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationInfo, RelationshipInfo, SchemaInfo, TriggerInfo,
    TxStep, UniqueKeyInfo,
};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tiberius::error::IoErrorKind;
use tiberius::{Client, ColumnType, Config};
use tokio::net::TcpStream;
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore, mpsc, oneshot,
};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// A SQL Server connection, as tiberius drives it once tokio's socket has been
/// adapted to the futures-io traits it wants.
type Tds = Client<Compat<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum MsSqlError {
    #[error("{}", describe(.0))]
    Tds(#[from] tiberius::error::Error),
    /// A server error read against the statement that produced it, which is the
    /// only way its line number can be turned into a place in that text.
    #[error("{}", describe(.error))]
    Statement {
        error: tiberius::error::Error,
        position: Option<u32>,
    },
    /// A statement that stopped because this driver killed its session.
    #[error("{}", describe(.0))]
    Cancelled(tiberius::error::Error),
    #[error(
        "column {column:?} has type {sql_type}, which this driver cannot read — cast it to text"
    )]
    UnsupportedType { column: String, sql_type: String },
    #[error("decimal value {value} does not fit a column of scale {scale}")]
    NumericOverflow { value: String, scale: i8 },
    #[error("a {expected} column sent {found}")]
    UnexpectedValue {
        expected: &'static str,
        found: &'static str,
    },
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("connection pool exhausted")]
    PoolExhausted,
    /// Deliberately without the value that failed: it is most often a password,
    /// and an error message is the one place it is certain to be both shown on
    /// screen and written to a log.
    #[error(
        "the {field} contains a character SQL Server's connection string cannot \
         carry ({character:?}); change it, or connect with a Server=…;Password=… string"
    )]
    Unquotable {
        field: &'static str,
        character: char,
    },
    /// The task reading a result went away before it said anything, which means
    /// the runtime dropped it rather than the server refusing.
    #[error("the task reading this result stopped before it produced anything")]
    ReaderGone,
}

impl From<std::io::Error> for MsSqlError {
    fn from(e: std::io::Error) -> Self {
        MsSqlError::Tds(e.into())
    }
}

impl MsSqlError {
    /// Where in the statement the server says the trouble is: 1-based, counted
    /// in characters.
    ///
    /// SQL Server does not answer the question the trait asks. `TokenError`
    /// carries a **line** number in the batch, not an offset into it, and there
    /// is no counterpart to PostgreSQL's character position. A line is converted
    /// to the offset of its first character, which points at the right line and
    /// no closer.
    ///
    /// That conversion is only made for a statement with more than one line. In
    /// a single-line statement "line 1" locates nothing, and a caret placed at
    /// character one because of it would be pointing confidently at whatever the
    /// statement happens to start with. No position is better than a wrong one.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            MsSqlError::Statement { position, .. } => *position,
            _ => None,
        }
    }

    /// Whether this statement stopped because somebody asked it to.
    ///
    /// PostgreSQL can ask the server: SQLSTATE 57014 means cancelled, and its
    /// driver's comment is explicit that the answer must not come from what this
    /// side remembers doing. SQL Server offers no equivalent. `KILL` ends the
    /// session, and severity 20 and above terminates the connection rather than
    /// returning a categorised error, so a killed statement arrives looking like
    /// the network dropping.
    ///
    /// So the answer is part memory and part evidence, and both halves are
    /// needed: this driver must have killed the session, *and* the failure must
    /// be one a kill produces. A remembered cancel on its own would mislabel a
    /// genuine fault that happened in the same moment; the error on its own
    /// would mislabel a pulled cable as a user's button press.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, MsSqlError::Cancelled(_))
    }
}

/// The message the server sent, rather than the wrapper tiberius puts round it.
///
/// `Error::Server` displays as "Token error: ..." with the server name and line
/// glued on, which is this client describing its own plumbing. What a user needs
/// is the sentence SQL Server wrote.
fn describe(e: &tiberius::error::Error) -> String {
    match e {
        tiberius::error::Error::Server(t) => t.message().to_string(),
        other => other.to_string(),
    }
}

/// The failure a statement produced, classified while both the statement text
/// and the knowledge of whether we killed it are still in reach.
///
/// Neither fact survives into `From<MsSqlError> for DbError`, which is why the
/// classification happens here rather than there.
fn fault(error: tiberius::error::Error, sql: &str, killed: bool) -> MsSqlError {
    if killed && looks_like_a_kill(&error) {
        return MsSqlError::Cancelled(error);
    }
    let position = match &error {
        tiberius::error::Error::Server(t) => line_offset(sql, t.line()),
        _ => None,
    };
    MsSqlError::Statement { error, position }
}

/// Whether a failure is one that killing a session produces.
///
/// Error 596 — "Cannot continue the execution because the session is in the kill
/// state" — is what a live SQL Server 2022 sends, at severity 21. Microsoft
/// publishes no reference page for that number, so it is matched here as
/// observed behaviour rather than as documented behaviour, and the integration
/// suite pins it so a server that stops sending it is noticed. The other two
/// shapes are what arrives when the connection is already gone by the time the
/// client looks: severity 20 and above terminates the connection outright, which
/// is also why `TRY…CATCH` cannot catch it.
fn looks_like_a_kill(e: &tiberius::error::Error) -> bool {
    match e {
        tiberius::error::Error::Server(t) => t.code() == 596 || t.class() >= 20,
        tiberius::error::Error::Io { kind, .. } => matches!(
            kind,
            IoErrorKind::ConnectionReset | IoErrorKind::UnexpectedEof | IoErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

/// The 1-based character offset at which `line` starts, or `None` when a line
/// number says nothing.
///
/// Characters and not bytes: the trait counts positions in characters, and the
/// difference is invisible until somebody names a table in a language that is
/// not English.
fn line_offset(sql: &str, line: u32) -> Option<u32> {
    // A statement of one line is entirely on line one, so the number adds
    // nothing to what the caller already knows.
    if line < 1 || !sql.contains('\n') {
        return None;
    }
    let mut offset: u32 = 1;
    for (n, text) in sql.split('\n').enumerate() {
        if n as u32 + 1 == line {
            return Some(offset);
        }
        // The newline that ended this line is a character of the statement too.
        offset += text.chars().count() as u32 + 1;
    }
    None
}

/// A connection string with encryption asked for, unless the caller said
/// otherwise.
///
/// tiberius defaults `encrypt` to off when the key is absent, which is the
/// opposite of every current Microsoft client. Left alone, a connection anybody
/// wrote by hand would carry its password and its results in plaintext without
/// saying so.
fn with_encryption_default(conn_str: &str) -> String {
    let mentions_encryption = conn_str
        .split(';')
        .filter_map(|part| part.split_once('='))
        .any(|(key, _)| key.trim().eq_ignore_ascii_case("encrypt"));
    if mentions_encryption {
        conn_str.to_string()
    } else {
        format!("{};Encrypt=true", conn_str.trim_end_matches(';'))
    }
}

/// The ADO connection string a `sqlserver://` URL is asking for.
///
/// Two spellings of the same fact, and this driver has to read both. The
/// connection form builds `scheme://user:password@host:port/database` for every
/// database the client can open, because a form that wanted a different shape
/// per driver would be a form per driver. SQL Server's own spelling is ADO's
/// `Server=tcp:host,port;…`, which is what SSMS and the Azure portal hand out
/// and therefore what somebody pastes.
///
/// Only the URL form is converted. An ADO string is passed through, since it is
/// already what tiberius reads.
fn ado_from_url(url: &str) -> Result<String, MsSqlError> {
    let Some(rest) = url.strip_prefix("sqlserver://") else {
        return Ok(url.to_string());
    };
    // Split from the right on `@` and from the left on `/`: the form
    // percent-encodes both characters inside a user name or a password, so the
    // last `@` really does end the credentials and the first `/` really does
    // begin the database name.
    let (credentials, rest) = match rest.rsplit_once('@') {
        Some((credentials, rest)) => (Some(credentials), rest),
        None => (None, rest),
    };
    // Anything a URL cannot say in its own grammar goes in the query string,
    // spelled the way ADO spells it: `?TrustServerCertificate=true` against a
    // development server with a self-signed certificate, `?Application Name=…`
    // to be recognisable in `sys.dm_exec_sessions`. Passing them through under
    // their own names means this does not become a second, smaller list of the
    // settings SQL Server has.
    let (rest, settings) = match rest.split_once('?') {
        Some((rest, settings)) => (rest, settings),
        None => (rest, ""),
    };
    let (authority, database) = match rest.split_once('/') {
        Some((authority, database)) => (authority, database),
        None => (rest, ""),
    };

    // `Server=tcp:host,port` is ADO's spelling of a TCP target; a bare host
    // means "look this instance up over UDP", which is a different thing that
    // fails on any server with the browser service turned off.
    let mut out = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            format!("Server=tcp:{host},{port}")
        }
        _ => format!("Server=tcp:{authority}"),
    };
    if !database.is_empty() {
        out.push_str(&format!(";Database={}", quoted("database", database)?));
    }
    if let Some(credentials) = credentials {
        let (user, password) = match credentials.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (credentials, None),
        };
        out.push_str(&format!(";User Id={}", quoted("user name", user)?));
        if let Some(password) = password {
            out.push_str(&format!(";Password={}", quoted("password", password)?));
        }
    }
    for setting in settings.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = setting.split_once('=').unwrap_or((setting, ""));
        out.push_str(&format!(
            ";{}={}",
            percent_decode(key),
            quoted("connection setting", value)?
        ));
    }
    Ok(out)
}

/// One percent-decoded field, wrapped in braces if ADO needs it to be.
///
/// `{}` is ADO's quoting, and it has no escape inside itself — the parser reads
/// to the first closing brace and stops. So a value holding one cannot be
/// expressed at all, and saying that is better than emitting a string that
/// parses into a different password than the one that was typed and comes back
/// as a login failure.
fn quoted(field: &'static str, value: &str) -> Result<String, MsSqlError> {
    let value = percent_decode(value);
    if let Some(character) = value.chars().find(|c| *c == '}') {
        return Err(MsSqlError::Unquotable { field, character });
    }
    if value.contains([';', '=', '{']) {
        Ok(format!("{{{value}}}"))
    } else {
        Ok(value)
    }
}

/// `%XX` turned back into the byte it stands for.
///
/// Written out rather than taken from a crate because this is the whole of what
/// is needed: the connection form percent-encodes what it builds, and nothing
/// else about URL syntax reaches here.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(high), Some(low)) => {
                match (
                    char::from(*high).to_digit(16),
                    char::from(*low).to_digit(16),
                ) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        i += 3;
                    }
                    // Not an escape after all, so it is a literal per cent sign.
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One connection, and the session id a cancel has to name.
///
/// The session id is read once at connect rather than looked up when it is
/// wanted, because by then the connection is busy with the statement the caller
/// is trying to stop and asking it anything would queue behind that.
struct Session {
    client: Tds,
    spid: i16,
}

impl Session {
    async fn connect(conn_str: &str) -> Result<Self, MsSqlError> {
        let config = Config::from_ado_string(conn_str)?;
        let tcp = TcpStream::connect(config.get_addr()).await?;
        // tiberius writes through a buffering Sink, so Nagle only adds latency.
        tcp.set_nodelay(true)?;
        let mut client = Client::connect(config, tcp.compat_write()).await?;
        // @@SPID is a smallint, so it is read as one; asking for an i32 fails to
        // convert rather than widening.
        let spid: i16 = client
            .simple_query("SELECT @@SPID")
            .await?
            .into_row()
            .await?
            .and_then(|r| r.get(0))
            .ok_or_else(|| {
                MsSqlError::Tds(tiberius::error::Error::Protocol(
                    "the server did not answer SELECT @@SPID".into(),
                ))
            })?;
        Ok(Session { client, spid })
    }
}

/// The statements a cancel could still reach.
///
/// A `KILL` ends a session, so it is only ever aimed at one that is running
/// something: a kill sent to an idle connection would destroy a browse nobody
/// asked to stop, and the contract requires cancelling an idle cursor to be a
/// no-op. "Running" is tracked from the consumer's side — a statement is busy
/// while somebody is waiting for something from it — because that is exactly the
/// state a Cancel button exists to interrupt.
///
/// Tickets rather than session ids, because the server reuses a spid once a
/// session ends and a killed spid left lying around would mislabel a later,
/// unrelated failure on the connection that inherited it.
#[derive(Default)]
struct Inflight {
    state: Mutex<InflightState>,
    next: AtomicU64,
}

#[derive(Default)]
struct InflightState {
    running: HashMap<u64, Entry>,
    killed: HashSet<u64>,
}

struct Entry {
    spid: i16,
    busy: Arc<AtomicBool>,
}

impl Inflight {
    fn register(&self, spid: i16) -> Ticket {
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        let busy = Arc::new(AtomicBool::new(false));
        self.state.lock().unwrap().running.insert(
            ticket,
            Entry {
                spid,
                busy: Arc::clone(&busy),
            },
        );
        Ticket { id: ticket, busy }
    }

    /// Every session that is busy right now, marked as killed on the way out.
    ///
    /// Marked before the `KILL` is sent, so that the failure it causes cannot
    /// arrive before the memory of having asked for it.
    fn claim_busy(&self, only: Option<u64>) -> Vec<i16> {
        let mut state = self.state.lock().unwrap();
        let claimed: Vec<(u64, i16)> = state
            .running
            .iter()
            .filter(|(ticket, entry)| {
                only.is_none_or(|t| t == **ticket) && entry.busy.load(Ordering::Acquire)
            })
            .map(|(ticket, entry)| (*ticket, entry.spid))
            .collect();
        for (ticket, _) in &claimed {
            state.killed.insert(*ticket);
        }
        claimed.into_iter().map(|(_, spid)| spid).collect()
    }

    fn was_killed(&self, ticket: u64) -> bool {
        self.state.lock().unwrap().killed.contains(&ticket)
    }

    fn forget(&self, ticket: u64) {
        let mut state = self.state.lock().unwrap();
        state.running.remove(&ticket);
        state.killed.remove(&ticket);
    }
}

/// One registered statement, which stops being reachable by a cancel when this
/// is dropped.
struct Ticket {
    id: u64,
    busy: Arc<AtomicBool>,
}

/// Sends `KILL` for each session, on a connection of its own.
///
/// A connection of its own for the reason the PostgreSQL driver gives about its
/// own cancel: TDS cannot interleave a second request on a busy connection, so
/// an in-band cancel would sit in the queue behind the statement it is trying to
/// stop.
async fn kill(conn_str: &str, spids: &[i16]) -> Result<(), MsSqlError> {
    let mut session = Session::connect(conn_str).await?;
    for spid in spids {
        // Not a bound parameter, because a session id is a syntactic element of
        // `KILL` and not an expression. The number is an `i16` this driver read
        // from `@@SPID`; no text a user typed can reach here.
        session
            .client
            .simple_query(format!("KILL {spid}"))
            .await?
            .into_results()
            .await?;
    }
    Ok(())
}

/// What one database on the instance is, for a front end that wants to offer a
/// connection to it.
///
/// Not a `SchemaInfo`: these are not reachable through this connection, and
/// listing them as though they were would put nodes in the navigator that cannot
/// be expanded.
#[derive(Debug, Clone)]
pub struct DatabaseInfo {
    pub name: String,
    /// `ONLINE`, `RESTORING`, `OFFLINE`, and so on, in the server's own words.
    pub state: String,
    pub collation: Option<String>,
}

/// A session against one SQL Server database.
///
/// Statements run on one connection and nothing else does, because a transaction
/// belongs to a connection: a `BEGIN` sent down a borrowed one opens a
/// transaction the next statement will not be given and nobody can commit. One
/// connection also means one statement at a time, so the second one waits — that
/// queueing is what a transaction costs, and it is why metadata reads take a
/// connection from a small pool instead of joining the queue. Expanding a schema
/// then does not sit behind a result that is still streaming.
///
/// A cursor is the exception and takes a connection of its own. Its pages come
/// off a statement left open, so on the session connection it would hold the
/// transaction hostage for the whole browse; the trait puts a cursor outside
/// whatever the session has open for that reason. The same arrangement, and the
/// same reasons, as the PostgreSQL driver's.
pub struct MsSqlSource {
    conn_str: String,
    database: String,
    /// The connection statements run on, empty when the last one left it
    /// unusable.
    ///
    /// `Option` rather than a `Session`, because this driver cancels by ending
    /// the session: a `KILL` leaves a socket that will never answer again, and
    /// handing it to the next statement would turn one cancelled statement into
    /// every statement after it failing. Emptied by whoever saw it die, filled
    /// again by whoever wants it next — which is also where a server that is
    /// down can report itself, rather than in a reconnect nobody asked for.
    session: Arc<AsyncMutex<Option<Session>>>,
    pool: Arc<Mutex<Vec<Session>>>,
    semaphore: Arc<Semaphore>,
    inflight: Arc<Inflight>,
}

/// The session connection, held for the length of one statement.
struct Held {
    slot: OwnedMutexGuard<Option<Session>>,
}

impl Held {
    /// Filled by `MsSqlSource::hold` before this value exists, and only ever
    /// emptied by `discard`, which consumes it — so it is `Some` for the whole
    /// life of any reference taken through here.
    fn session(&mut self) -> &mut Session {
        self.slot.as_mut().unwrap()
    }

    fn spid(&self) -> i16 {
        self.slot.as_ref().unwrap().spid
    }

    /// Gives the connection up rather than back, for a statement that ended it.
    fn discard(mut self) {
        *self.slot = None;
    }
}

/// Where a statement's connection came from, and what becomes of it afterwards.
enum Lease {
    /// The session connection, returned when the statement ends so that the next
    /// one — and any transaction it is inside — finds it where it was left.
    Session(Held),
    /// A connection of the statement's own, closed with it. What a cursor takes:
    /// it holds its connection for as long as somebody is paging, which is not a
    /// thing the session connection can be asked to do.
    ///
    /// Boxed because a tiberius `Client` is the better part of a kilobyte and
    /// the other variant is a pointer. This value is moved into a task for every
    /// statement, so the unboxed enum would copy that kilobyte each time to hold
    /// something a cursor alone ever uses.
    Own(Box<Session>),
}

impl Lease {
    fn session(&mut self) -> &mut Session {
        match self {
            Lease::Session(held) => held.session(),
            Lease::Own(session) => session,
        }
    }

    fn spid(&self) -> i16 {
        match self {
            Lease::Session(held) => held.spid(),
            Lease::Own(session) => session.spid,
        }
    }

    /// Ends the lease, keeping the connection only if it can still be used.
    fn release(self, reusable: bool) {
        match self {
            Lease::Session(held) if !reusable => held.discard(),
            // Either the session connection going back into its slot, or a
            // statement's own connection closing with it. Both are a drop.
            _ => {}
        }
    }
}

/// Whether a failure leaves the connection unusable.
///
/// Asked because the session connection outlives the statement that failed on
/// it, so the answer decides whether the next statement inherits it. A statement
/// that failed on its own merits — a syntax error, a missing table — left the
/// session exactly as it was, transaction included, and throwing the connection
/// away for that would roll back work nobody asked to abandon. What does end a
/// connection is what `looks_like_a_kill` describes, and for the same reason it
/// describes it: severity 20 and above terminates the connection outright, and an
/// I/O error means it is already gone.
fn ends_the_connection(e: &MsSqlError) -> bool {
    match e {
        MsSqlError::Cancelled(_) => true,
        MsSqlError::Tds(inner) | MsSqlError::Statement { error: inner, .. } => {
            looks_like_a_kill(inner)
        }
        _ => false,
    }
}

/// A pooled connection, borrowed for one call and returned when it goes out of
/// scope.
struct Pooled {
    session: Option<Session>,
    pool: Arc<Mutex<Vec<Session>>>,
    _permit: OwnedSemaphorePermit,
}

impl Deref for Pooled {
    type Target = Tds;

    fn deref(&self) -> &Self::Target {
        // Taken only by drop, so it is Some for the whole life of any reference
        // a caller can hold.
        &self.session.as_ref().unwrap().client
    }
}

impl DerefMut for Pooled {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session.as_mut().unwrap().client
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        let session = self.session.take().unwrap();
        self.pool.lock().unwrap().push(session);
    }
}

impl MsSqlSource {
    /// Opens a session from either spelling of a SQL Server connection string.
    ///
    /// `sqlserver://sa:password@host:1433/database`, which is what the
    /// connection form builds, or ADO's own
    /// `Server=tcp:host,port;Database=db;User Id=sa;Password=…`, which is what
    /// SSMS and the Azure portal hand out. Encryption is turned on when the
    /// string does not mention it, which is the opposite of what tiberius does
    /// when left alone.
    pub async fn connect(conn_str: &str) -> Result<Self, MsSqlError> {
        let conn_str = with_encryption_default(&ado_from_url(conn_str)?);
        // Opened eagerly so a wrong password is a failure to connect rather than
        // a failure at the first metadata call.
        let mut session = Session::connect(&conn_str).await?;
        let database: String = session
            .client
            .simple_query("SELECT DB_NAME()")
            .await?
            .into_row()
            .await?
            .and_then(|r| r.get::<&str, _>(0).map(str::to_string))
            .unwrap_or_default();

        Ok(Self {
            conn_str,
            database,
            // The connection that was opened to check the password becomes the
            // one statements run on. The pool starts empty and opens its first
            // connection when a metadata call wants one.
            session: Arc::new(AsyncMutex::new(Some(session))),
            pool: Arc::new(Mutex::new(Vec::new())),
            semaphore: Arc::new(Semaphore::new(4)),
            inflight: Arc::new(Inflight::default()),
        })
    }

    /// Takes the session connection, waiting for the statement before it.
    ///
    /// Opens one when the slot is empty, which is how a session that was killed
    /// — or a connection that dropped — is replaced. The replacement carries no
    /// transaction, because the server ended the old one when it ended the
    /// session.
    async fn hold(&self) -> Result<Held, MsSqlError> {
        let mut slot = Arc::clone(&self.session).lock_owned().await;
        if slot.is_none() {
            *slot = Some(Session::connect(&self.conn_str).await?);
        }
        Ok(Held { slot })
    }

    /// The one database this connection can see.
    ///
    /// A SQL Server instance holds many and this reaches one of them. The front
    /// end labels the navigator root with this so that a bare schema name is not
    /// left looking like it means the same thing on every connection.
    pub fn database(&self) -> &str {
        &self.database
    }

    async fn acquire(&self) -> Result<Pooled, MsSqlError> {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| MsSqlError::PoolExhausted)?;
        let existing = self.pool.lock().unwrap().pop();
        let session = match existing {
            Some(session) => session,
            None => Session::connect(&self.conn_str).await?,
        };
        Ok(Pooled {
            session: Some(session),
            pool: Arc::clone(&self.pool),
            _permit: permit,
        })
    }

    /// Every database on the instance, so a front end can offer a connection to
    /// one of them.
    ///
    /// Not reachable through this connection: cross-database three-part names
    /// work on a box-product server and do not work on Azure SQL Database, where
    /// each database is an isolated container. Offering a second connection is
    /// the answer that is right on both.
    pub async fn databases(&self) -> Result<Vec<DatabaseInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::databases(&mut conn).await
    }

    /// Schemas of the database this connection named.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::schemas(&mut conn).await
    }

    /// Tables and views within a schema.
    pub async fn relations(&self, schema: &str) -> Result<Vec<RelationInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::relations(&mut conn, schema).await
    }

    /// Column definitions for one relation.
    pub async fn columns(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ColumnInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::columns(&mut conn, schema, relation).await
    }

    /// The statement a view is defined by; `None` for a relation that has none.
    pub async fn definition(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Option<String>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::definition(&mut conn, schema, relation).await
    }

    /// Indexes on one relation, primary key first.
    pub async fn indexes(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<IndexInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::indexes(&mut conn, schema, relation).await
    }

    /// UNIQUE constraints on one relation, primary key excluded.
    pub async fn unique_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<UniqueKeyInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::unique_keys(&mut conn, schema, relation).await
    }

    /// Foreign keys this relation declares.
    pub async fn foreign_keys(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::foreign_keys(&mut conn, schema, relation).await
    }

    /// Foreign keys other relations declare against this one.
    pub async fn referenced_by(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<RelationshipInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::referenced_by(&mut conn, schema, relation).await
    }

    /// CHECK and UNIQUE constraints.
    pub async fn constraints(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<ConstraintInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::constraints(&mut conn, schema, relation).await
    }

    /// Triggers somebody wrote, excluding the ones the server ships.
    pub async fn triggers(
        &self,
        schema: &str,
        relation: &str,
    ) -> Result<Vec<TriggerInfo>, MsSqlError> {
        let mut conn = self.acquire().await?;
        metadata::triggers(&mut conn, schema, relation).await
    }

    /// Runs `sql` and streams its result in batches of at most `batch_rows`.
    ///
    /// Resolves once the server has described the result, which for a statement
    /// that produces rows means after the first `COLMETADATA` has arrived — so
    /// the grid can be laid out before a single row is read. An execution
    /// failure can still surface from `next_batch`.
    ///
    /// On the session connection, which is what lets a transaction opened by one
    /// statement still be there for the next. The cost is that one connection
    /// carries one statement at a time, so this waits for the result before it
    /// to be read to the end or let go of — a front end that keeps a result open
    /// and asks for another keeps the second one waiting.
    pub async fn query(&self, sql: &str, batch_rows: usize) -> Result<ArrowStream, MsSqlError> {
        let lease = Lease::Session(self.hold().await?);
        self.start(lease, sql, batch_rows).await
    }

    /// Reads `sql` forward, a page at a time.
    ///
    /// The mechanism is a statement left open on a connection of its own, with
    /// pages handed out from the token stream as it arrives. Page two is a
    /// continuation of page one's read rather than a second execution, so it
    /// cannot repeat or skip a row, and it needs no key, no `ORDER BY` and no
    /// transaction — which matters, because the requirement includes paging a
    /// heap.
    ///
    /// `OFFSET`/`FETCH` is the obvious alternative and is not used. Microsoft
    /// documents it as stable only when every page runs in one snapshot or
    /// serializable transaction *and* the `ORDER BY` is over columns guaranteed
    /// unique; the `ORDER BY (SELECT NULL)` everybody reaches for guarantees
    /// nothing, and on a keyless table there is no expression that fixes it.
    ///
    /// The connection is the cursor's own and not the session's, which is what
    /// the trait means by a cursor being outside whatever the session has open:
    /// a browse is held for as long as somebody is looking at it, and a
    /// transaction that could not be committed until then would be a transaction
    /// held open by a scrollbar.
    pub async fn cursor(&self, sql: &str, batch_rows: usize) -> Result<Cursor, MsSqlError> {
        let lease = Lease::Own(Box::new(Session::connect(&self.conn_str).await?));
        Ok(Cursor {
            stream: self.start(lease, sql, batch_rows).await?,
        })
    }

    /// Asks the server to abandon whatever this session is running.
    ///
    /// Aimed only at connections that are actually busy. That is a stronger
    /// filter than the PostgreSQL driver applies, and it has to be: PostgreSQL
    /// cancels a statement and leaves the connection alive, so naming an idle
    /// backend costs a round trip and does nothing, whereas `KILL` on an idle
    /// connection destroys it. Cancelling with nothing running is a no-op that
    /// succeeds, which is what a Cancel button pressed at the wrong moment has
    /// to do.
    ///
    /// Best-effort. Success means the requests were delivered, not that anything
    /// stopped; what actually happened shows up where the statement is, as a
    /// failure whose `is_cancelled` is true.
    pub async fn cancel(&self) -> Result<(), MsSqlError> {
        let targets = self.inflight.claim_busy(None);
        if targets.is_empty() {
            return Ok(());
        }
        kill(&self.conn_str, &targets).await
    }

    /// Takes one step of transaction control, on the connection statements run
    /// on.
    ///
    /// That connection and no other, which is the whole reason this driver keeps
    /// one. The words are T-SQL's rather than the standard's — `SAVE
    /// TRANSACTION` where PostgreSQL writes `SAVEPOINT` — so they live here and
    /// not in the caller.
    ///
    /// Sent as a batch and not through `execute`, which is not a stylistic
    /// choice: tiberius' `execute` wraps the statement in `sp_executesql`, and a
    /// `BEGIN TRANSACTION` that opens inside a procedure and does not close
    /// before it returns is error 266 — the transaction survives and the caller
    /// is told it failed.
    ///
    /// Nothing here touches `SET IMPLICIT_TRANSACTIONS`. SQL Server commits each
    /// statement on its own unless one of these has opened a transaction, which
    /// is exactly what `TxStep` describes; it has no step for autocommit, and
    /// turning implicit transactions on would open one after every commit
    /// without anybody having asked for it.
    pub async fn transaction(&self, step: &TxStep) -> Result<(), MsSqlError> {
        let statement = match step {
            TxStep::Begin => "BEGIN TRANSACTION".to_string(),
            TxStep::Commit => "COMMIT TRANSACTION".to_string(),
            TxStep::Rollback => "ROLLBACK TRANSACTION".to_string(),
            TxStep::Savepoint(name) => format!("SAVE TRANSACTION {name}"),
            TxStep::RollbackTo(name) => format!("ROLLBACK TRANSACTION {name}"),
            // T-SQL has no RELEASE, and this is not a step SQL Server is missing
            // — it is a step it does not need. A savepoint here is a mark in the
            // log with no resources of its own, kept until the transaction that
            // holds it ends. Nothing is left open by not releasing it, so this
            // succeeds rather than refusing a caller who paired a savepoint with
            // a release and did nothing wrong.
            //
            // One difference is worth stating rather than hiding: the standard
            // says a released savepoint can no longer be rolled back to, and SQL
            // Server will still accept `ROLLBACK TRANSACTION <name>` afterwards.
            // This driver promises less than the standard there, not more.
            TxStep::Release(_) => return Ok(()),
        };

        let mut held = self.hold().await?;
        let outcome = match held.session().client.simple_query(&statement).await {
            // Drained rather than dropped: a batch reports its failures in the
            // token stream, so a step that was refused looks like a success
            // until somebody reads to the end of it.
            Ok(stream) => stream.into_results().await.map(drop),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                let e = MsSqlError::Tds(e);
                // The same rule the statement path follows: a connection that
                // did not survive is given up rather than handed on.
                if ends_the_connection(&e) {
                    held.discard();
                }
                Err(e)
            }
        }
    }

    /// Registers a statement's connection so a cancel can find it, and starts
    /// reading.
    async fn start(
        &self,
        lease: Lease,
        sql: &str,
        batch_rows: usize,
    ) -> Result<ArrowStream, MsSqlError> {
        let batch_rows = batch_rows.max(1);
        // Registered before the statement is sent, so there is no window in
        // which something is running that a cancel cannot see.
        let ticket = self.inflight.register(lease.spid());

        let (schema_tx, schema_rx) = oneshot::channel();
        // One batch in flight. The reader stops on a full channel, so a result
        // stops growing when the front end stops reading it — the bound
        // expressed as backpressure rather than as an eviction policy.
        let (batch_tx, batches) = mpsc::channel(1);
        let rows_affected = Arc::new(AtomicI64::new(-1));

        let owned_sql = sql.to_string();
        let affected = Arc::clone(&rows_affected);
        let inflight = Arc::clone(&self.inflight);
        let id = ticket.id;
        tokio::spawn(async move {
            pump(
                lease, owned_sql, batch_rows, schema_tx, batch_tx, affected, inflight, id,
            )
            .await;
        });

        // Busy until the server has answered, because until then the caller is
        // waiting on a statement that a Cancel button should be able to stop.
        ticket.busy.store(true, Ordering::Release);
        let schema = schema_rx.await.map_err(|_| MsSqlError::ReaderGone);
        ticket.busy.store(false, Ordering::Release);

        Ok(ArrowStream {
            schema: schema??,
            batches,
            rows_affected,
            ticket,
            inflight: Arc::clone(&self.inflight),
            conn_str: self.conn_str.clone(),
        })
    }
}

/// What the server said about a statement before it ran.
///
/// Three questions in one round trip: does it name a type that would crash the
/// decoder, what precision does each decimal column have, and does it produce a
/// result set at all.
enum Described {
    Columns(Vec<DescribedColumn>),
    /// The server would not analyse the statement. That is ordinary — a batch
    /// that builds a temp table and selects from it cannot be described until it
    /// runs — and it means the two answers above are unavailable, not that the
    /// statement is wrong.
    Unknown,
}

struct DescribedColumn {
    name: String,
    type_name: String,
    /// The TDS type byte the server will send this column under.
    system_type_id: i32,
    decimal: Option<(u8, i8)>,
}

/// TDS type bytes tiberius cannot decode: `UDTTYPE` covers `geometry`,
/// `geography`, `hierarchyid` and any CLR type somebody registered themselves,
/// and `SSVARIANTTYPE` covers `sql_variant`. Matched on the byte rather than on
/// the type's name so a user-defined CLR type is caught too.
const TDS_UDT: i32 = 0xF0;
const TDS_SQL_VARIANT: i32 = 0x62;

/// Asks the server to describe a statement without running it.
///
/// This exists because reading a `geography`, `geometry`, `hierarchyid` or
/// `sql_variant` column panics inside tiberius while it parses the column
/// metadata — before any row, so there is no point at which the driver could
/// inspect the column and back out — and a panic in a release build of this
/// workspace aborts the process. Describing first turns that crash into a
/// message.
///
/// What it does not cover, stated plainly because it is the residual risk:
///
/// - `sys.dm_exec_describe_first_result_set` describes the **first** result set
///   only. A batch of several statements whose *second* result set has one of
///   these columns is still fatal.
/// - A batch the server declines to analyse — a temp table is the common case —
///   is not described at all, and is allowed through rather than refused,
///   because refusing every undescribable batch would break far more than it
///   protects.
///
/// - If the describe query itself fails — an older server, or a login without
///   the permission — nothing is known and the statement goes ahead.
///
/// Closing all three means patching tiberius to return an error where it
/// currently panics: four `todo!()` arms and one `unimplemented!()`. That needs
/// a `[patch.crates-io]` entry in the workspace root, which is a file this crate
/// may not touch, so it is a recommendation rather than a change.
async fn describe_statement(client: &mut Tds, sql: &str) -> Described {
    // No `is_hidden` filter: browse information is off, so the server adds no
    // hidden columns, and a `WHERE` here would also drop the rows that carry the
    // error number this has to see.
    let query = "SELECT r.name, r.system_type_name, r.system_type_id, \
                        r.precision, r.scale, r.error_number \
                 FROM sys.dm_exec_describe_first_result_set(@P1, NULL, 0) AS r \
                 ORDER BY r.column_ordinal";
    let Ok(stream) = client.query(query, &[&sql]).await else {
        // Not knowing is not the same as knowing there is nothing wrong. This
        // is the fail-open half of the trade above, and it is taken because a
        // driver that refused every statement it could not vet would be
        // unusable against the servers and logins where this call is not
        // available.
        return Described::Unknown;
    };
    let Ok(rows) = stream.into_first_result().await else {
        return Described::Unknown;
    };

    let mut columns = Vec::with_capacity(rows.len());
    for row in &rows {
        // A row carrying an error number is the server saying it could not work
        // the statement out, which includes the ordinary case of it being
        // syntactically wrong. Executing it will say so far better than this
        // could.
        if row.get::<i32, _>(5).is_some() {
            return Described::Unknown;
        }
        let precision: Option<u8> = row.get(3);
        let scale: Option<u8> = row.get(4);
        columns.push(DescribedColumn {
            name: row.get::<&str, _>(0).unwrap_or_default().to_string(),
            type_name: row.get::<&str, _>(1).unwrap_or_default().to_string(),
            system_type_id: row.get(2).unwrap_or(0),
            decimal: decimal_layout(precision, scale),
        });
    }
    Described::Columns(columns)
}

/// A precision and scale pair Arrow will accept, or nothing.
///
/// Arrow refuses a `Decimal128` whose precision is outside 1..=38 or whose scale
/// is larger than its precision, and a builder constructed from a pair it
/// refuses panics. SQL Server should never state one, so this is a guard rather
/// than a conversion.
fn decimal_layout(precision: Option<u8>, scale: Option<u8>) -> Option<(u8, i8)> {
    let (precision, scale) = (precision?, scale?);
    ((1..=38).contains(&precision) && scale <= precision).then_some((precision, scale as i8))
}

impl Described {
    /// The first column whose type would crash the decoder.
    fn unreadable(&self) -> Option<(&str, &str)> {
        let Described::Columns(columns) = self else {
            return None;
        };
        columns
            .iter()
            .find(|c| matches!(c.system_type_id, TDS_UDT | TDS_SQL_VARIANT))
            .map(|c| (c.name.as_str(), c.type_name.as_str()))
    }

    /// Whether the server is sure this statement returns no result set.
    ///
    /// Only `Some(true)` when the server described the statement and found no
    /// columns in it. A statement it would not describe answers `None`, because
    /// "I do not know" and "there are no rows" are different answers and only
    /// one of them justifies reporting a row count.
    fn produces_no_rows(&self) -> Option<bool> {
        match self {
            Described::Columns(columns) => Some(columns.is_empty()),
            Described::Unknown => None,
        }
    }

    /// The declared decimal layout of each column, by position.
    fn decimals(&self) -> Vec<Option<(u8, i8)>> {
        match self {
            Described::Columns(columns) => columns.iter().map(|c| c.decimal).collect(),
            Described::Unknown => Vec::new(),
        }
    }
}

/// Reads one statement to the end, sending its schema and then its batches.
#[allow(clippy::too_many_arguments)]
async fn pump(
    mut lease: Lease,
    sql: String,
    batch_rows: usize,
    schema_tx: oneshot::Sender<Result<SchemaRef, MsSqlError>>,
    batch_tx: mpsc::Sender<Result<RecordBatch, MsSqlError>>,
    rows_affected: Arc<AtomicI64>,
    inflight: Arc<Inflight>,
    ticket: u64,
) {
    let mut schema_tx = Some(schema_tx);
    let outcome = read(
        lease.session(),
        &sql,
        batch_rows,
        &mut schema_tx,
        &batch_tx,
        &rows_affected,
    )
    .await
    // Classified here, while both halves of the answer are still in reach: the
    // statement text, which is what turns a line number into a place, and the
    // ticket, which is what says whether this driver killed the session out from
    // under it.
    .map_err(|e| classify(&inflight, ticket, e, &sql));

    let reusable = match &outcome {
        Ok(()) => true,
        Err(e) => !ends_the_connection(e),
    };
    if let Err(e) = outcome {
        // Which way the failure goes out depends on how far this got: a
        // statement that never produced a schema failed at `query`, and one that
        // did fails at the batch the caller is waiting for. Both are dropped
        // silently when nobody is listening any more, which is the ordinary end
        // of a result the front end let go of.
        match schema_tx.take() {
            Some(tx) => drop(tx.send(Err(e))),
            None => drop(batch_tx.send(Err(e)).await),
        }
    }
    // Forgotten before the connection is given back, and that order is the whole
    // point: the session connection is about to be somebody else's, and a ticket
    // still holding its session id could send a `KILL` to the statement that
    // inherited it.
    inflight.forget(ticket);
    // A result the caller stopped reading leaves rows on the wire. Handing that
    // connection on is safe because tiberius drains a dirty stream before its
    // next statement — the one thing it does do about a stream let go of early,
    // and the reason a partly read result is not a reason to throw a session
    // away.
    lease.release(reusable);
}

async fn read(
    session: &mut Session,
    sql: &str,
    batch_rows: usize,
    schema_tx: &mut Option<oneshot::Sender<Result<SchemaRef, MsSqlError>>>,
    batch_tx: &mpsc::Sender<Result<RecordBatch, MsSqlError>>,
    rows_affected: &AtomicI64,
) -> Result<(), MsSqlError> {
    let described = describe_statement(&mut session.client, sql).await;
    if let Some((column, sql_type)) = described.unreadable() {
        return Err(MsSqlError::UnsupportedType {
            column: column.to_string(),
            sql_type: sql_type.to_string(),
        });
    }

    if described.produces_no_rows() == Some(true) {
        // A statement that writes says what it did in its DONE token, and
        // tiberius surfaces that count from `execute` and not from a query
        // stream. Asking the server first which kind of statement this is means
        // the number reported is the rows it changed rather than the zero rows
        // it returned — the same reason the SQLite driver asks a statement
        // whether it writes before believing a count.
        let outcome = session.client.execute(sql, &[]).await?;
        let total: u64 = outcome.rows_affected().iter().sum();
        rows_affected.store(total as i64, Ordering::Release);
        if let Some(tx) = schema_tx.take() {
            drop(tx.send(Ok(Arc::new(Schema::empty()))));
        }
        return Ok(());
    }

    let decimals = described.decimals();
    let mut stream = session.client.simple_query(sql).await?;
    let columns = stream.columns().await?.unwrap_or_default().to_vec();
    let layouts: Vec<ColumnLayout> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| ColumnLayout {
            column_type: c.column_type(),
            // Only decimals need it, and only where the describe agreed about
            // how many columns there are. A mismatch means the two views of the
            // statement disagree, and the fallback layout is better than reading
            // a scale off the wrong column.
            decimal: match c.column_type() {
                ColumnType::Decimaln | ColumnType::Numericn => decimals.get(i).copied().flatten(),
                _ => None,
            },
        })
        .collect();
    let fields = columns
        .iter()
        .zip(&layouts)
        .map(|(c, l)| arrow_field(c.name(), l))
        .collect::<Result<Vec<_>, _>>()?;
    let schema: SchemaRef = Arc::new(Schema::new(fields));
    if let Some(tx) = schema_tx.take() {
        // Nobody left to tell means the caller gave up between asking and being
        // answered, which is not a failure of the statement.
        if tx.send(Ok(Arc::clone(&schema))).is_err() {
            return Ok(());
        }
    }

    let mut rows = stream.into_row_stream();
    let mut produced: i64 = 0;
    loop {
        let mut builders: Vec<ColBuilder> = layouts
            .iter()
            .map(|l| ColBuilder::new(l, batch_rows))
            .collect();
        let mut n = 0usize;
        let mut done = false;
        while n < batch_rows {
            match rows.next().await {
                Some(row) => {
                    let row = row?;
                    // A batch of several statements produces several result sets
                    // on one stream, and the trait has one schema per result. So
                    // this reads the first and stops: rows of the second would be
                    // a different shape wearing the first one's column names.
                    if row.result_index() != 0 {
                        done = true;
                        break;
                    }
                    for (data, b) in row.cells().map(|(_, d)| d).zip(builders.iter_mut()) {
                        b.append(data)?;
                    }
                    n += 1;
                    produced += 1;
                }
                None => {
                    done = true;
                    break;
                }
            }
        }
        if n > 0 {
            let arrays = builders.iter_mut().map(|b| b.finish()).collect();
            let batch = RecordBatch::try_new(Arc::clone(&schema), arrays)?;
            // A closed channel is the consumer having let the result go, which
            // ends the read rather than failing it.
            if batch_tx.send(Ok(batch)).await.is_err() {
                return Ok(());
            }
        }
        if done {
            break;
        }
    }
    rows_affected.store(produced, Ordering::Release);
    Ok(())
}

/// A result being read forward, one batch at a time.
pub struct ArrowStream {
    schema: SchemaRef,
    batches: mpsc::Receiver<Result<RecordBatch, MsSqlError>>,
    rows_affected: Arc<AtomicI64>,
    ticket: Ticket,
    inflight: Arc<Inflight>,
    conn_str: String,
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows the statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Two numbers under one name, as the other drivers also report: rows
    /// changed for a statement that writes, rows produced for one that reads.
    /// Which one this is was settled before the statement ran, by asking the
    /// server whether it produces a result set at all — a statement tiberius
    /// streams has no row count to read, because the count rides on a DONE token
    /// the query stream does not surface.
    pub fn rows_affected(&self) -> Option<u64> {
        u64::try_from(self.rows_affected.load(Ordering::Acquire)).ok()
    }

    /// Next batch, or `None` once the result is fully consumed.
    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, MsSqlError> {
        // Busy for exactly as long as somebody is waiting on a batch that has
        // not arrived, which is the window a Cancel button exists for. Outside
        // it there is nothing running for a `KILL` to stop, and killing anyway
        // would destroy a result the user is still reading.
        self.ticket.busy.store(true, Ordering::Release);
        let next = self.batches.recv().await;
        self.ticket.busy.store(false, Ordering::Release);
        match next {
            Some(batch) => batch.map(Some),
            // The sender is gone, which is how the reader says it reached the
            // end. An error would have arrived through the channel first.
            None => Ok(None),
        }
    }

    fn canceller(&self) -> CursorCancel {
        CursorCancel {
            conn_str: self.conn_str.clone(),
            inflight: Arc::clone(&self.inflight),
            ticket: self.ticket.id,
        }
    }
}

impl Drop for ArrowStream {
    fn drop(&mut self) {
        // Letting go of a result takes it out of `cancel`'s reach: the reader
        // task notices the closed channel, stops, and drops the connection.
        self.inflight.forget(self.ticket.id);
    }
}

/// A result read a page at a time.
///
/// The pages come off one statement left open on a connection of its own, so
/// page two is the continuation of page one's read and cannot repeat or skip a
/// row. Holding the connection is what that costs.
pub struct Cursor {
    stream: ArrowStream,
}

impl Cursor {
    pub fn schema(&self) -> SchemaRef {
        self.stream.schema()
    }

    /// Next page, or `None` once the cursor has reached the end.
    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, MsSqlError> {
        self.stream.next_batch().await
    }

    /// A handle for stopping this cursor's fetch from another thread.
    ///
    /// Taken out here rather than reached for at cancel time, because by then
    /// the cursor is borrowed by the fetch that is to be stopped — which is the
    /// whole situation.
    ///
    /// Using it ends the browse rather than pausing it. `KILL` terminates the
    /// session, so the statement the pages were coming from is gone and the
    /// cursor has no more pages to give; there is no SQL Server equivalent of
    /// `pg_cancel_backend`, which leaves the connection usable.
    pub fn canceller(&self) -> CursorCancel {
        self.stream.canceller()
    }

    /// Closes the cursor and releases the connection behind it.
    ///
    /// Optional: dropping it does the same. Closing the channel is what tells
    /// the reader to stop, and the reader ending is what closes the connection
    /// and with it the statement.
    pub async fn close(&mut self) -> Result<(), MsSqlError> {
        self.stream.batches.close();
        // Drained rather than merely closed, so the reader is not left blocked
        // on a send nobody will ever take.
        while self.stream.batches.recv().await.is_some() {}
        Ok(())
    }
}

/// Stops the fetch one cursor is running.
pub struct CursorCancel {
    conn_str: String,
    inflight: Arc<Inflight>,
    ticket: u64,
}

impl CursorCancel {
    /// Delivered is not interrupted: a fetch that had already finished leaves
    /// nothing to stop and this still succeeds.
    pub async fn cancel(&self) -> Result<(), MsSqlError> {
        let targets = self.inflight.claim_busy(Some(self.ticket));
        if targets.is_empty() {
            return Ok(());
        }
        kill(&self.conn_str, &targets).await
    }
}

/// Turns the failure a statement produced into one that knows where it is and
/// whether it was asked for.
fn classify(inflight: &Inflight, ticket: u64, e: MsSqlError, sql: &str) -> MsSqlError {
    match e {
        MsSqlError::Tds(inner) | MsSqlError::Statement { error: inner, .. } => {
            fault(inner, sql, inflight.was_killed(ticket))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs no database — it needs the absence of one, which is why it can run
    /// in the unit suite. Port 1 is reserved and nothing listens there.
    #[tokio::test]
    async fn a_connection_that_never_happened_says_why_not() {
        let err = MsSqlSource::connect(
            "Server=tcp:127.0.0.1,1;Database=nothing;User Id=nobody;Password=x",
        )
        .await
        .err()
        .expect("nothing is listening on port 1");
        assert!(
            err.to_string().to_lowercase().contains("refused"),
            "expected the refusal to survive into the message, got: {err}"
        );
    }

    #[test]
    fn a_url_becomes_the_ado_string_it_stands_for() {
        assert_eq!(
            ado_from_url("sqlserver://sa:Str0ng!Passw0rd@localhost:51433/bench").unwrap(),
            "Server=tcp:localhost,51433;Database=bench;User Id=sa;Password=Str0ng!Passw0rd"
        );
        // No port, no database, no credentials: each part is optional, and a
        // missing one has to leave no trace rather than an empty setting, which
        // ADO reads as "the empty string" instead of "not said".
        assert_eq!(
            ado_from_url("sqlserver://localhost").unwrap(),
            "Server=tcp:localhost"
        );
        // Integrated security, which is a user with no password rather than no
        // user at all.
        assert_eq!(
            ado_from_url("sqlserver://sa@localhost:1433/db").unwrap(),
            "Server=tcp:localhost,1433;Database=db;User Id=sa"
        );
    }

    #[test]
    fn a_query_string_becomes_the_settings_it_names() {
        // The escape hatch for everything a URL has no field for. Against a
        // development server with a self-signed certificate this is the
        // difference between connecting and not.
        assert_eq!(
            ado_from_url(
                "sqlserver://sa@host:1433/db?TrustServerCertificate=true&Application%20Name=dbclient"
            )
            .unwrap(),
            "Server=tcp:host,1433;Database=db;User Id=sa\
             ;TrustServerCertificate=true;Application Name=dbclient"
        );
    }

    #[test]
    fn an_ado_string_is_left_exactly_as_it_was_given() {
        // The form somebody pastes from SSMS. Rewriting it would mean parsing
        // it, and it is already what tiberius reads.
        let given = "Server=tcp:localhost,1433;Database=db;User Id=sa;Password=x";
        assert_eq!(ado_from_url(given).unwrap(), given);
    }

    #[test]
    fn a_password_that_would_break_the_string_is_quoted_instead() {
        // A semicolon ends a setting and an equals sign ends a key, so a
        // password holding either has to be braced or it becomes several
        // settings, none of which is the password.
        let out = ado_from_url("sqlserver://sa:a%3Bb%3Dc@localhost:1433/db").unwrap();
        assert!(out.ends_with(";Password={a;b=c}"), "got {out}");
    }

    #[test]
    fn a_password_no_ado_string_can_carry_is_refused_before_the_attempt() {
        // ADO's braces have no escape inside themselves — the parser reads to
        // the first closing brace and stops — so this cannot be expressed. The
        // failure has to say so, because the alternative is a login failure for
        // a password that was typed correctly.
        let err = ado_from_url("sqlserver://sa:a%7Db@localhost:1433/db")
            .expect_err("a closing brace cannot be carried");
        assert!(err.to_string().contains("password"), "got: {err}");
        assert!(
            !err.to_string().contains("a}b"),
            "the password leaked: {err}"
        );
    }

    #[test]
    fn encryption_is_asked_for_when_the_caller_did_not_say() {
        // tiberius defaults this off, which is the opposite of every current
        // Microsoft client: left alone, a hand-written connection string sends
        // its password in the clear.
        let out = with_encryption_default("Server=tcp:localhost,1433;Database=db");
        assert!(out.ends_with(";Encrypt=true"), "got {out}");
    }

    #[test]
    fn an_explicit_encryption_setting_is_left_alone() {
        for given in [
            "Server=x;Encrypt=false",
            "Server=x;encrypt=DANGER_PLAINTEXT",
            "Server=x;ENCRYPT=true;Database=d",
        ] {
            assert_eq!(with_encryption_default(given), given);
        }
        // A trailing separator is not a second Encrypt key.
        assert_eq!(
            with_encryption_default("Server=x;"),
            "Server=x;Encrypt=true"
        );
    }

    #[test]
    fn a_single_line_statement_has_no_position_to_give() {
        // SQL Server reports a line, not an offset. In a one-line statement the
        // line is always 1, which locates nothing; a caret placed at character
        // one because of it points confidently at the wrong character.
        assert_eq!(
            line_offset("SELECT id FROM nums WHERE ORDER BY id", 1),
            None
        );
    }

    #[test]
    fn a_line_number_becomes_the_offset_that_line_starts_at() {
        let sql = "SELECT 1\nFROM nums\nWHERE ORDER BY id";
        assert_eq!(line_offset(sql, 1), Some(1));
        assert_eq!(line_offset(sql, 2), Some(10));
        assert_eq!(line_offset(sql, 3), Some(20));
        assert_eq!(line_offset(sql, 4), None);
    }

    #[test]
    fn an_offset_counts_characters_and_not_bytes() {
        // 客戶 is two characters and six bytes. The first line is 22 characters
        // and 26 bytes, so counting bytes would put the caret four characters
        // past the line it belongs to — invisible until somebody names a table
        // in a language that is not English.
        let sql = "SELECT * FROM sales.客戶\nWHERE ORDER BY id";
        assert_eq!(line_offset(sql, 2), Some(24));
        assert_eq!(sql.chars().count(), 22 + 1 + 17);
        assert_eq!(sql.len(), 26 + 1 + 17);
    }

    #[test]
    fn a_position_lands_inside_the_statement_it_came_from() {
        let sql = "SELECT 1\nFROM nums\nWHERE ORDER BY id";
        for line in 1..=6 {
            if let Some(p) = line_offset(sql, line) {
                assert!(p >= 1, "positions count from one, got {p}");
                assert!(p as usize <= sql.chars().count() + 1);
            }
        }
    }

    #[test]
    fn a_decimal_layout_the_server_could_not_state_is_declined() {
        assert_eq!(decimal_layout(Some(18), Some(4)), Some((18, 4)));
        assert_eq!(decimal_layout(Some(38), Some(38)), Some((38, 38)));
        assert_eq!(decimal_layout(None, Some(4)), None);
        // Arrow refuses these, and a builder made from a pair it refuses panics.
        assert_eq!(decimal_layout(Some(0), Some(0)), None);
        assert_eq!(decimal_layout(Some(39), Some(2)), None);
        assert_eq!(decimal_layout(Some(4), Some(8)), None);
    }
}
