//! C ABI surface. The only crate a front-end links against.
//!
//! Result data crosses via the Arrow C Data Interface, so handing a batch to
//! Swift is passing two structs — no serialization, no copy. That property is
//! what Phase 0 exists to verify, so it is load-bearing rather than an
//! optimization.
//!
//! Calls are synchronous and must not be made from the UI thread. Phase 1
//! replaces this with a handle-plus-event-queue design; Phase 0 keeps it
//! blocking because a background dispatch queue on the Swift side is enough to
//! measure with, and the simpler surface is easier to trust.

mod registry;
mod session;

use arrow::array::{Array, RecordBatch, StructArray};
use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use dbcatalog::{Kind, Names};
use dbconn::{Cursor, CursorCancel, Driver, ResultStream};
use dbsql::{Dialect, Origin, TokenKind};
use dbtunnel::{Credential, Tunnel, TunnelConfig};
use session::Session;
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("failed to start tokio runtime"))
}

/// How many suggestions cross the ABI before the rest are dropped.
///
/// A schema with two thousand tables answers `FROM wide.` with two thousand
/// names, and the popup shows ten of them. Serialising the other one thousand
/// nine hundred and ninety is a cost paid on every keystroke for a list nobody
/// reaches the end of — a caret keeps moving, and one more letter is a cheaper
/// way to find a name than scrolling past a thousand of them.
///
/// The price is that names past the cap are absent without saying so. That is
/// the reason for a number this far above what fits on screen: it is high
/// enough that reaching it means the query was never specific enough to answer.
const SUGGESTION_CAP: usize = 1000;

/// Writes `msg` into `*err` as a freshly allocated C string, if `err` is non-null.
/// Caller releases it with `db_string_free`.
unsafe fn set_err(err: *mut *mut c_char, msg: impl std::fmt::Display) {
    if err.is_null() {
        return;
    }
    let c = CString::new(msg.to_string()).unwrap_or_else(|_| CString::new("error").unwrap());
    unsafe { *err = c.into_raw() };
}

/// One open session, whichever database is behind it.
///
/// The driver is chosen by the scheme of the connection string and never
/// mentioned again: everything below this line is written against the trait, so
/// adding a database adds an arm to `registry` and nothing here.
///
/// The names live on the handle because that is the only thing whose lifetime
/// they match. They are what a connection was told when it asked, so they are
/// wrong for a different connection and gone when this one closes; a cache
/// anywhere else would need to be told which of those two just happened.
pub struct DbHandle {
    driver: Arc<dyn Driver>,
    names: Names,
    session: Session,
    /// The dialect this connection is actually in, where this build knows it.
    ///
    /// `Names` carries the editor's answer, which guesses PostgreSQL for a
    /// database it does not know — a wrong guess there costs colour. This is the
    /// answer for everything that generates SQL rather than painting it, where a
    /// wrong guess costs a statement: without it a MongoDB collection renders as
    /// a PostgreSQL `CREATE TABLE`, which is the failure `dbddl::for_dialect`
    /// exists to refuse and cannot see coming from one level up.
    dialect: Option<&'static Dialect>,
    /// The forward this connection was opened through, or `None` when the
    /// database was dialled directly.
    ///
    /// Never read, and here for its `Drop`: closing the forward is what the
    /// tunnel's lifetime means. What that costs is worth writing down, because
    /// it is what makes the failure hard to place. The socket the driver
    /// already dialled goes on working — what closes is the forward, not the
    /// connection through it — so the session keeps answering. It is the *next*
    /// connection that cannot be made, which for this driver is the first
    /// metadata call, and it arrives as a refusal from 127.0.0.1 that explains
    /// nothing.
    #[allow(dead_code)]
    tunnel: Option<Tunnel>,
}

pub struct DbQuery {
    stream: Box<dyn ResultStream>,
}

/// The canceller is a field of its own rather than something reached for through
/// the cursor, because it is used at exactly the moment the cursor is borrowed:
/// `db_cursor_cancel` runs while a `db_cursor_next` is in flight on another
/// thread. Each entry point therefore borrows only the field it needs — the two
/// are disjoint, so neither has to know the other is running.
pub struct DbCursor {
    cursor: Box<dyn Cursor>,
    cancel: Box<dyn CursorCancel>,
}

/// An SSH bastion to reach the database through, as the caller fills it in.
///
/// Flat fields rather than a tagged union, because this struct is written by
/// hand on the other side of the ABI and a union is a shape that can be filled
/// in half-way without a compiler anywhere noticing. What flatness costs is a
/// pair of fields that must not both be set, and that is checked here rather
/// than trusted — see `bastion_of`.
#[repr(C)]
pub struct DbSshConfig {
    pub host: *const c_char,
    pub port: u16,
    pub user: *const c_char,
    /// Exactly one of these two is set.
    pub password: *const c_char,
    pub key_path: *const c_char,
    /// Null for a key that is not encrypted.
    pub passphrase: *const c_char,
    /// The file the bastion's identity is checked against, in full. Named by
    /// the caller rather than found here: which `known_hosts` applies is a
    /// question about whose account is running, and this crate cannot see that.
    pub known_hosts: *const c_char,
}

/// Reads one of that struct's strings.
///
/// Null and empty are both "not filled in". A form hands over the empty string
/// for a field nobody typed in, and a bastion configured with an empty user is
/// not a different thing from one configured with no user at all.
///
/// # Safety
/// `p` is null or a valid NUL-terminated C string.
unsafe fn ssh_field<'a>(p: *const c_char, name: &str) -> Result<Option<&'a str>, String> {
    if p.is_null() {
        return Ok(None);
    }
    match unsafe { CStr::from_ptr(p) }.to_str() {
        Ok("") => Ok(None),
        Ok(s) => Ok(Some(s)),
        Err(_) => Err(format!("the SSH {name} is not valid text")),
    }
}

/// Turns the caller's struct into what the tunnel takes, or nothing at all when
/// there is no bastion.
///
/// Every failure here is a struct filled in wrong rather than a server that
/// said no, and each gets its own sentence. The alternative — quietly dialling
/// the database directly when the bastion is half-configured — is the failure
/// worth spending the words on: it succeeds on the one network where the
/// database was reachable anyway and fails everywhere else, which is the
/// hardest kind of fault to be told about.
///
/// # Safety
/// `ssh` is null, or points at a `DbSshConfig` whose strings are valid.
unsafe fn bastion_of(ssh: *const DbSshConfig) -> Result<Option<TunnelConfig>, String> {
    if ssh.is_null() {
        return Ok(None);
    }
    let ssh = unsafe { &*ssh };
    let host =
        unsafe { ssh_field(ssh.host, "host")? }.ok_or("an SSH bastion needs a host to reach")?;
    let user = unsafe { ssh_field(ssh.user, "user")? }
        .ok_or("an SSH bastion needs a user to log in as")?;
    let known_hosts = unsafe { ssh_field(ssh.known_hosts, "known_hosts file")? }
        .ok_or("an SSH bastion needs a known_hosts file to check its identity against")?;
    if ssh.port == 0 {
        return Err("an SSH bastion needs a port".into());
    }
    let password = unsafe { ssh_field(ssh.password, "password")? };
    let key_path = unsafe { ssh_field(ssh.key_path, "key file")? };
    let credential = match (password, key_path) {
        (Some(password), None) => Credential::Password(password.to_owned()),
        (None, Some(path)) => Credential::Key {
            path: PathBuf::from(path),
            passphrase: unsafe { ssh_field(ssh.passphrase, "passphrase")? }.map(str::to_owned),
        },
        (Some(_), Some(_)) => {
            return Err("an SSH bastion takes a password or a key file, not both".into());
        }
        (None, None) => return Err("an SSH bastion needs a password or a key file".into()),
    };
    Ok(Some(TunnelConfig {
        host: host.to_owned(),
        port: ssh.port,
        user: user.to_owned(),
        credential,
        known_hosts: PathBuf::from(known_hosts),
    }))
}

/// Opens the database `conn_str` names.
///
/// The string starts with the driver it wants — `postgres://…`, `sqlite://…` —
/// and there is no fallback for one that does not. A bare `host=… port=…` is a
/// PostgreSQL string today and a MySQL string in the same shape tomorrow, and a
/// client that guesses between them is one that connects to the wrong database
/// without saying so.
///
/// `timeout_secs` bounds the whole attempt, not the TCP connection inside it.
/// That is the case worth bounding: a server that accepts the socket and then
/// never finishes the handshake is indistinguishable, from here, from one that
/// is merely slow, and the driver beneath has no reason of its own to stop
/// waiting. 0 means wait as long as it takes, which is not a mode the
/// application asks for — it is here so that a caller with its own limit is not
/// made to invent a second one.
///
/// `ssh` is a bastion to reach the database through, or null to dial it
/// directly — which is what almost every connection does. The forward it opens
/// belongs to the handle that comes back and closes when that handle is freed.
///
/// # Safety
/// `conn_str` must be a valid NUL-terminated C string, and `ssh` must be null
/// or point at a `DbSshConfig` whose strings are the same.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_connect(
    conn_str: *const c_char,
    ssh: *const DbSshConfig,
    timeout_secs: u32,
    err: *mut *mut c_char,
) -> *mut DbHandle {
    if conn_str.is_null() {
        unsafe { set_err(err, "conn_str is null") };
        return ptr::null_mut();
    }
    let s = match unsafe { CStr::from_ptr(conn_str) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    // Before anything is dialled, so that a bastion filled in wrong is answered
    // here rather than by a connection that quietly went straight to the
    // database instead.
    let bastion = match unsafe { bastion_of(ssh) } {
        Ok(bastion) => bastion,
        Err(message) => {
            unsafe { set_err(err, message) };
            return ptr::null_mut();
        }
    };
    let opened = runtime().block_on(async move {
        // The one door every connection in the application goes through, with a
        // bastion or without one. A second entry point for the tunnelled case
        // would be a path exercised only by the connections somebody remembered
        // to send through it.
        let attempt = registry::connect_through(s, bastion);
        if timeout_secs == 0 {
            return attempt.await;
        }
        match tokio::time::timeout(Duration::from_secs(u64::from(timeout_secs)), attempt).await {
            Ok(outcome) => outcome,
            // Deliberately without the connection string, for the reason the
            // parse failure above is: it holds a password, and this message is
            // certain to be shown on screen. What is worth saying is that the
            // limit was reached rather than that the database refused, because
            // those two send somebody to different places to look.
            Err(_) => Err(dbconn::DbError::new(format!(
                "the database did not answer within {timeout_secs}s"
            ))),
        }
    });
    match opened {
        Ok((driver, tunnel)) => {
            let driver: Arc<dyn Driver> = Arc::from(driver);
            let scheme = registry::scheme_of(s);
            let names = Names::new(driver.clone(), dbsql::for_scheme(scheme));
            Box::into_raw(Box::new(DbHandle {
                driver,
                names,
                session: Session::new(),
                dialect: dbsql::of_scheme(scheme),
                tunnel,
            }))
        }
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_free(handle: *mut DbHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// Asks the server to stop whatever this handle is currently running. Returns 0
/// when the request was delivered, -1 when it could not be.
///
/// The one call here that may be made while another is in flight on the same
/// handle, and it has to be: everything else blocks, so a cancel that waited its
/// turn would arrive after the statement it exists to interrupt. Sound because
/// the cancel travels on a connection of its own and touches nothing the running
/// call owns.
///
/// A handle is a session rather than a connection, and may be several of them —
/// which one is busy is not something the caller can see, and not something the
/// caller should have to. A cursor is the exception: it is handed out to be
/// held, so it carries its own `db_cursor_cancel`.
///
/// Delivery is not interruption. A statement that finished first, or that was
/// never running, leaves the server nothing to cancel and this still returns 0.
/// The outcome is observable only where the statement is: `db_query_next`
/// answering -2.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed. It must not be
/// freed concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cancel(handle: *mut DbHandle, err: *mut *mut c_char) -> c_int {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return -1;
    }
    let h = unsafe { &*handle };
    match runtime().block_on(h.driver.cancel()) {
        Ok(()) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Serializes `value` as JSON into a caller-owned C string.
///
/// Metadata crosses as JSON rather than Arrow: it is a few thousand short rows
/// at most, so the encoding cost is irrelevant and the Swift side stays a
/// `JSONDecoder` call instead of a column reader.
fn json_result<T: serde::Serialize>(value: &T, err: *mut *mut c_char) -> *mut c_char {
    match serde_json::to_string(value) {
        Ok(s) => match CString::new(s) {
            Ok(c) => c.into_raw(),
            Err(e) => {
                unsafe { set_err(err, e) };
                ptr::null_mut()
            }
        },
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Every database this build can open, as a JSON array. Release with
/// `db_string_free`.
///
/// Takes no handle, because it answers a question asked before there is one: the
/// connection form has to know which databases to offer, and what each of them
/// needs asked for. Exported rather than duplicated in Swift so that a driver
/// added to the core appears in the form without anybody remembering to add it
/// twice — and so the form cannot offer one this build does not have.
///
/// # Safety
/// `err` must be null or point to a writable `*mut c_char`. It is only written
/// on failure, and what it is set to must be released with `db_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_drivers_json(err: *mut *mut c_char) -> *mut c_char {
    json_result(&registry::CATALOG, err)
}

/// What answered this connection — product and version — as a JSON object.
/// Release with `db_string_free`.
///
/// A round trip, unlike the call above it: that one answers what this build can
/// open, and this one answers what it actually opened. They sit together because
/// a front end reads them for the same reason, and because without this one the
/// only way to learn that a `postgres://` connection reached CockroachDB is to
/// read it out of an error message.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_server_info_json(
    handle: *mut DbHandle,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    match runtime().block_on(h.driver.server_info()) {
        Ok(v) => json_result(&v, err),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// What this connection can do, as a JSON object. Release with `db_string_free`.
///
/// Takes a handle rather than a scheme, which is the whole reason it is a call
/// and not a table: the MySQL driver reaches StarRocks and Doris as well as
/// MySQL, and those two are not transactional. A front end keyed on `mysql://`
/// would be wrong for exactly the products the scheme cannot tell apart.
///
/// No I/O — everything in it was settled when the connection opened — so this is
/// priced like an accessor and may be asked whenever it is convenient.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_capabilities_json(
    handle: *mut DbHandle,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    json_result(&h.driver.capabilities(), err)
}

/// Asks the database whether this connection is still good. 0 if it is, -1 if
/// it is not, with `err` set.
///
/// A real round trip on the session's own connection, which is the only thing
/// that answers the question being asked: a TCP socket stays open long after the
/// server behind it has stopped, and the operating system will not say so until
/// something is written. `server_info` is what it sends, because every driver
/// already has one and each has already chosen the cheapest thing its database
/// will answer.
///
/// It queues behind whatever the session is running, since that connection is
/// serial. A caller that pings while a statement is in flight will therefore
/// wait for the statement — so ask when the connection is idle, which is also
/// the only time the answer means anything.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_ping(handle: *mut DbHandle, err: *mut *mut c_char) -> c_int {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return -1;
    }
    let h = unsafe { &*handle };
    match runtime().block_on(h.driver.server_info()) {
        Ok(_) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Everything an editor asks about one buffer of SQL, in one answer.
///
/// The three questions — what to paint, where the statements are, and which one
/// a run would send — are asked about the same text at the same moment, and the
/// scan that answers any of them has already answered the other two. Three entry
/// points would be three scans of the same buffer for every keystroke.
#[derive(serde::Serialize)]
struct Scan {
    /// Kind, start and end for every token, in that order and flattened. An
    /// array of objects would spend most of the payload repeating three field
    /// names, and this is the part a keystroke pays for.
    tokens: Vec<u32>,
    /// Start and end for every statement, flattened for the same reason.
    statements: Vec<u32>,
    /// Absent for a buffer with nothing in it to run.
    target: Option<RunTarget>,
}

/// What a run would send, flattened so that the caller decodes one shape rather
/// than three.
#[derive(serde::Serialize)]
struct RunTarget {
    start: u32,
    end: u32,
    /// `whole`, `statement` or `selection`.
    origin: &'static str,
    /// Which statement of how many, both counted from 1. Zero for the two
    /// origins that number nothing, which is unambiguous because there is no
    /// zeroth statement.
    index: u32,
    of: u32,
}

impl From<dbsql::Target> for RunTarget {
    fn from(target: dbsql::Target) -> Self {
        let (origin, index, of) = match target.origin {
            Origin::Whole => ("whole", 0, 0),
            Origin::Statement { index, of } => ("statement", index as u32, of as u32),
            Origin::Selection => ("selection", 0, 0),
        };
        RunTarget {
            start: target.span.start,
            end: target.span.end,
            origin,
            index,
            of,
        }
    }
}

/// The number a token kind crosses as.
///
/// Written out rather than taken from the enum's discriminant, because the order
/// the variants happen to sit in is a promise to nobody: a reordering that
/// silently painted every string literal as a comment is the kind of mistake a
/// person notices and a compiler does not.
fn token_code(kind: TokenKind) -> u32 {
    match kind {
        TokenKind::Terminator => 0,
        TokenKind::Keyword => 1,
        TokenKind::Identifier => 2,
        TokenKind::QuotedIdentifier => 3,
        TokenKind::String => 4,
        TokenKind::DollarQuoted => 5,
        TokenKind::Number => 6,
        TokenKind::Comment => 7,
        TokenKind::Parameter => 8,
        TokenKind::Whitespace => 9,
        TokenKind::Other => 10,
    }
}

/// One reading of an editor buffer, as JSON. Release with `db_string_free`.
///
/// Takes no handle, like `db_drivers_json` and for a related reason: reading SQL
/// needs the dialect and not the connection, and an editor holds text before
/// anything is open. `scheme` is the connection's — `postgres`, `mysql`,
/// `sqlite` — and one this build does not know is read as PostgreSQL rather than
/// refused, because a wrong guess there costs colour and not correctness.
///
/// Offsets in and out are counted in characters from zero, which is the unit a
/// Swift `String.unicodeScalars` index is. `selection_start` and `selection_end`
/// are equal for a caret; either order is accepted, since a front end that hands
/// them over backwards means the same span.
///
/// # Safety
/// `text` and `scheme` must be valid NUL-terminated C strings. `err` must be
/// null or point to a writable `*mut c_char`, and what it is set to must be
/// released with `db_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_sql_scan_json(
    text: *const c_char,
    scheme: *const c_char,
    selection_start: u32,
    selection_end: u32,
    err: *mut *mut c_char,
) -> *mut c_char {
    if text.is_null() || scheme.is_null() {
        unsafe { set_err(err, "null text or scheme") };
        return ptr::null_mut();
    }
    let (text, scheme) = unsafe {
        match (
            CStr::from_ptr(text).to_str(),
            CStr::from_ptr(scheme).to_str(),
        ) {
            (Ok(t), Ok(s)) => (t, s),
            _ => {
                set_err(err, "text or scheme is not valid UTF-8");
                return ptr::null_mut();
            }
        }
    };

    let dialect = dbsql::for_scheme(scheme);
    let selection = selection_start.min(selection_end)..selection_start.max(selection_end);
    let read = dbsql::scan(text, selection, dialect);
    let scan = Scan {
        tokens: read
            .tokens
            .iter()
            .flat_map(|t| [token_code(t.kind), t.start, t.end])
            .collect(),
        statements: read
            .statements
            .iter()
            .flat_map(|s| [s.start, s.end])
            .collect(),
        target: read.target.map(RunTarget::from),
    };
    json_result(&scan, err)
}

/// `text` laid out again, as a caller-owned C string to release with
/// `db_string_free`.
///
/// Takes no scheme, unlike its neighbours here. The formatter treats every
/// quoted region — backticks, `[brackets]`, `$tag$…$tag$` — as one opaque token
/// whichever database wrote it, so there is no dialect for it to be told.
///
/// Never fails on the text itself: SQL it cannot read comes back as it arrived,
/// because this runs on a buffer somebody is editing and the worst outcome is
/// not an ugly result but a lost one.
///
/// # Safety
/// `text` must be a valid NUL-terminated C string; `err` must be null or point
/// to a writable `*mut c_char`, released with `db_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_sql_format(text: *const c_char, err: *mut *mut c_char) -> *mut c_char {
    if text.is_null() {
        unsafe { set_err(err, "null text") };
        return ptr::null_mut();
    }
    let Ok(text) = (unsafe { CStr::from_ptr(text) }).to_str() else {
        unsafe { set_err(err, "text is not valid UTF-8") };
        return ptr::null_mut();
    };
    match CString::new(dbsql::format(text)) {
        Ok(c) => c.into_raw(),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// How `scheme`'s database asks for a query plan, or NULL where it cannot be
/// asked at all.
///
/// NULL is an answer rather than a failure, which is why this takes no `err`. A
/// scheme this build does not know and a dialect with no prefix mean the same
/// thing to a caller — do not offer the command — and neither is anything the
/// user can act on.
///
/// Answered through `of_scheme` rather than `for_scheme` for the reason that
/// function's own comment gives: a caller for whom a wrong guess costs more than
/// colour wants the honest `None`. This is one, since handing MongoDB the word
/// `EXPLAIN` would produce a statement that cannot run.
///
/// # Safety
/// `scheme` must be null or a valid NUL-terminated C string. The returned string
/// is the caller's, released with `db_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_sql_explain_prefix(scheme: *const c_char) -> *mut c_char {
    if scheme.is_null() {
        return ptr::null_mut();
    }
    let Ok(scheme) = (unsafe { CStr::from_ptr(scheme) }).to_str() else {
        return ptr::null_mut();
    };
    let Some(prefix) = dbsql::of_scheme(scheme).and_then(|d| d.explain_prefix) else {
        return ptr::null_mut();
    };
    CString::new(prefix).map_or(ptr::null_mut(), CString::into_raw)
}

/// What running `text` would do: `safe`, `modify`, `dangerous` or `fatal`.
/// Release with `db_string_free`.
///
/// Takes no handle, like `db_sql_scan_json`: the answer is read from the SQL and
/// the dialect, and the question is asked before anything is sent. The answer is
/// the worst of the statements in `text`, because a script goes out statement by
/// statement and every one of them lands.
///
/// Takes no `err` either, like `db_sql_explain_prefix` above: there is no failure
/// this can report that a caller could act on. NULL means the text could not be
/// read at all, which is a caller's bug rather than a user's.
///
/// Read from each statement's head and nothing else — `crates/sql/src/danger.rs`
/// says why that limit is deliberate. It is enough to decide whether to ask a
/// question, and it is not a promise about what the server will do with what it
/// is sent.
///
/// # Safety
/// `text` and `scheme` must be valid NUL-terminated C strings. The returned
/// string is the caller's, released with `db_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_sql_danger(text: *const c_char, scheme: *const c_char) -> *mut c_char {
    if text.is_null() || scheme.is_null() {
        return ptr::null_mut();
    }
    let (Ok(text), Ok(scheme)) = (
        unsafe { CStr::from_ptr(text) }.to_str(),
        unsafe { CStr::from_ptr(scheme) }.to_str(),
    ) else {
        return ptr::null_mut();
    };
    let word = dbsql::script_danger(text, dbsql::for_scheme(scheme)).name();
    CString::new(word).map_or(ptr::null_mut(), CString::into_raw)
}

/// Where a server's error position lands in the buffer, or -1 when the number
/// could not have come from what was sent.
///
/// A position is counted from 1, in characters, and from the start of the string
/// the server was handed — which is one statement, not the buffer it was cut
/// from. Exported rather than left to each front end because the rule for where
/// such a number stops being believable is the scanner's, and a second copy of
/// it is a second chance to be one character out.
///
/// Takes no pointers and cannot fail, so it has no `err` and no unsafety.
#[unsafe(no_mangle)]
pub extern "C" fn db_sql_error_offset(position: c_int, sent_start: u32, sent_end: u32) -> i64 {
    let Ok(position) = u32::try_from(position) else {
        return -1;
    };
    dbsql::error_offset(position, &(sent_start..sent_end)).map_or(-1, i64::from)
}

/// What could be typed at the caret, best first.
#[derive(serde::Serialize)]
struct Offers {
    /// The characters accepting one of these replaces, which are the ones
    /// already typed of the name. Empty — `start == end` — where nothing has
    /// been typed yet.
    ///
    /// Answered here rather than left to the front end because deciding where a
    /// name begins is the lexer's rule: a quoted `"Order Lines"` is one name and
    /// a front end walking back over word characters would replace half of it.
    start: u32,
    end: u32,
    offers: Vec<Offer>,
}

/// One thing that could be typed.
#[derive(serde::Serialize)]
struct Offer {
    /// What to show: the name as the catalog holds it.
    label: String,
    /// What to put in the buffer, quoted if this database needs it to be.
    insert: String,
    /// `keyword`, `schema`, `relation`, `column` or `local`.
    kind: &'static str,
    /// The second line: a column's type, a relation's schema and kind.
    detail: String,
}

/// The name a suggestion's kind crosses as.
///
/// Words rather than numbers, unlike a token kind, because there are five of
/// them and they are read once per popup instead of once per character — the
/// payload can afford to say what it means.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Keyword => "keyword",
        Kind::Schema => "schema",
        Kind::Relation => "relation",
        Kind::Column => "column",
        Kind::Local => "local",
    }
}

/// What could be typed at `caret` in `text`, as JSON. Release with
/// `db_string_free`.
///
/// Takes a handle where `db_sql_scan_json` does not, because this is the half of
/// completion that needs the catalog: the question — column, relation, or
/// nothing at all — is answered from the text alone, and the names that answer
/// it belong to one connection.
///
/// `caret` is counted in characters from zero, as everywhere else on this
/// surface. It may sit past the end of `text`, which is what a front end that
/// rounds a selection hands over, and is answered rather than refused.
///
/// The first question a connection asks costs the metadata round trips it takes
/// to learn the names; every one after it is answered from memory until
/// `db_names_forget`. Blocking, like every other call here, so it belongs off
/// the UI thread — the first one is a network call wearing a keystroke's
/// clothes.
///
/// # Safety
/// `handle` must be live; `text` must be a valid NUL-terminated C string. `err`
/// must be null or point to a writable `*mut c_char`, and what it is set to must
/// be released with `db_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_complete_json(
    handle: *mut DbHandle,
    text: *const c_char,
    caret: u32,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || text.is_null() {
        unsafe { set_err(err, "null handle or text") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let text = match unsafe { CStr::from_ptr(text) }.to_str() {
        Ok(t) => t,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };

    let question = dbsql::complete(text, caret, h.names.dialect());
    let mut suggestions = runtime().block_on(h.names.suggest(&question));
    // Cut here rather than in the catalog: the catalog's ranking is what decides
    // which thousand survive, so it has to have ranked all of them first.
    suggestions.truncate(SUGGESTION_CAP);
    let offers = Offers {
        start: question.span.start,
        end: question.span.end,
        offers: suggestions
            .into_iter()
            .map(|s| Offer {
                label: s.label,
                insert: s.insert,
                kind: kind_name(s.kind),
                detail: s.detail,
            })
            .collect(),
    };
    json_result(&offers, err)
}

/// Forgets the names this connection has been told, so the next completion asks
/// the server again.
///
/// For the refresh a user presses. Nothing here expires on a timer: a table
/// appearing in the list at a moment nobody chose is worse than one that is a
/// few minutes stale, and the user is the one who knows a migration just ran.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_names_forget(handle: *mut DbHandle) {
    if handle.is_null() {
        return;
    }
    let h = unsafe { &*handle };
    runtime().block_on(h.names.forget());
}

/// The databases on this server as a JSON array, or JSON `null` where the
/// engine has no level above schemas. Release with `db_string_free`.
///
/// `null` and `[]` are different answers and both are reachable: SQLite has no
/// database level at all, while a SQL Server login that can see none of them
/// has the level and an empty list. A front end that collapsed the two would
/// draw a level with nothing under it, or hide one that exists.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_databases_json(
    handle: *mut DbHandle,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    match runtime().block_on(h.driver.databases()) {
        Ok(v) => json_result(&v, err),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Non-system schemas as a JSON array. Release with `db_string_free`.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_schemas_json(
    handle: *mut DbHandle,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    match runtime().block_on(h.driver.schemas()) {
        Ok(v) => json_result(&v, err),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Relations in `schema` as a JSON array. Release with `db_string_free`.
///
/// # Safety
/// `handle` must be live; `schema` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_relations_json(
    handle: *mut DbHandle,
    schema: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || schema.is_null() {
        unsafe { set_err(err, "null handle or schema") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let s = match unsafe { CStr::from_ptr(schema) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    match runtime().block_on(h.driver.relations(s)) {
        Ok(v) => json_result(&v, err),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Defines one `(handle, schema, relation, err) -> JSON` entry point.
///
/// The relation-scoped metadata calls differ only in which method they
/// dispatch to. Written out one at a time, the null checks and the UTF-8
/// handling become three copies of the same unsafe block — three places for
/// one mistake to be fixed in two of them.
macro_rules! relation_metadata {
    ($(#[$doc:meta])* $name:ident => $method:ident) => {
        $(#[$doc])*
        ///
        /// # Safety
        /// `handle` must be live; `schema` and `relation` must be valid
        /// NUL-terminated C strings.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            handle: *mut DbHandle,
            schema: *const c_char,
            relation: *const c_char,
            err: *mut *mut c_char,
        ) -> *mut c_char {
            if handle.is_null() || schema.is_null() || relation.is_null() {
                unsafe { set_err(err, "null handle, schema, or relation") };
                return ptr::null_mut();
            }
            let h = unsafe { &*handle };
            let (s, r) = unsafe {
                match (
                    CStr::from_ptr(schema).to_str(),
                    CStr::from_ptr(relation).to_str(),
                ) {
                    (Ok(s), Ok(r)) => (s, r),
                    _ => {
                        set_err(err, "schema or relation is not valid UTF-8");
                        return ptr::null_mut();
                    }
                }
            };
            match runtime().block_on(h.driver.$method(s, r)) {
                Ok(v) => json_result(&v, err),
                Err(e) => {
                    unsafe { set_err(err, e) };
                    ptr::null_mut()
                }
            }
        }
    };
}

relation_metadata! {
    /// Columns of one relation as a JSON array. Release with `db_string_free`.
    db_columns_json => columns
}

relation_metadata! {
    /// Indexes on one relation as a JSON array. Release with `db_string_free`.
    db_indexes_json => indexes
}

relation_metadata! {
    /// The statement a view is defined by, as a JSON string, or JSON `null` for
    /// a relation that has none. Release with `db_string_free`.
    db_definition_json => definition
}

relation_metadata! {
    /// Foreign keys of one relation as a JSON array. Release with `db_string_free`.
    db_foreign_keys_json => foreign_keys
}

relation_metadata! {
    /// Foreign keys pointing at one relation, as a JSON array. Release with
    /// `db_string_free`.
    db_referenced_by_json => referenced_by
}

relation_metadata! {
    /// CHECK, UNIQUE and EXCLUDE constraints as a JSON array. Release with
    /// `db_string_free`.
    db_constraints_json => constraints
}

relation_metadata! {
    /// User-defined triggers as a JSON array. Release with `db_string_free`.
    db_triggers_json => triggers
}

/// What a browse is asking for, as it crosses the boundary.
///
/// Owned, because the JSON it is decoded from is a C string this side does not
/// keep; the borrowed [`dbconn::Browse`] is built from it a line later.
#[derive(serde::Deserialize)]
struct BrowseRequest {
    schema: String,
    relation: String,
    filter: Option<String>,
    order: Option<String>,
    /// Absent means "no key columns", which is a relation with no primary key
    /// rather than a caller who forgot.
    #[serde(default)]
    keys: Vec<String>,
    limit: Option<u32>,
}

/// The statement that reads one relation's rows, as plain text. Release with
/// `db_string_free`.
///
/// Written and not run, like `db_edit_sql_json`: what comes back goes to the
/// server through `db_cursor` or `db_query`, and can be shown to whoever is
/// about to run it.
///
/// `what` is one relation and the filter bar's two fields:
///
/// ```json
/// {"schema": …, "relation": …, "filter": …, "order": …, "keys": […], "limit": …}
/// ```
///
/// The driver writes it, which is the whole point of this call existing: a front
/// end that assembled `SELECT * FROM "schema"."relation"` was writing PostgreSQL
/// for every database it could open, and MySQL reads those quotes as a string
/// while MongoDB has no SELECT at all.
///
/// # Safety
/// `handle` must be live; `what` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_browse_statement(
    handle: *mut DbHandle,
    what: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || what.is_null() {
        unsafe { set_err(err, "null handle or browse request") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let text = match unsafe { CStr::from_ptr(what) }.to_str() {
        Ok(text) => text,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    let requested: BrowseRequest = match serde_json::from_str(text) {
        Ok(requested) => requested,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    let statement = h.driver.browse(&dbconn::Browse {
        schema: &requested.schema,
        relation: &requested.relation,
        filter: requested.filter.as_deref(),
        order: requested.order.as_deref(),
        keys: &requested.keys,
        limit: requested.limit,
    });
    match CString::new(statement) {
        Ok(text) => text.into_raw(),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// The statements that would recreate one relation, as plain text. Release with
/// `db_string_free`.
///
/// Text and not JSON, unlike everything else that crosses as a string here: this
/// is one value rather than a record, and wrapping it would make the caller
/// decode a document to reach the only field in it.
///
/// The kind is read here rather than taken as an argument, at the cost of one
/// metadata call. A caller that passed it would be passing back something this
/// side told it, and the day the two disagree the answer is a `CREATE TABLE` for
/// a view — which is a statement that runs and makes the wrong object.
///
/// Fails for a database whose DDL has not been written yet, and for a kind whose
/// statement cannot be assembled from the metadata this core carries. Both say
/// so; neither guesses.
///
/// # Safety
/// `handle` must be live; `schema` and `relation` must be valid NUL-terminated C
/// strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_ddl_text(
    handle: *mut DbHandle,
    schema: *const c_char,
    relation: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || schema.is_null() || relation.is_null() {
        unsafe { set_err(err, "null handle, schema, or relation") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let (s, r) = unsafe {
        match (
            CStr::from_ptr(schema).to_str(),
            CStr::from_ptr(relation).to_str(),
        ) {
            (Ok(s), Ok(r)) => (s, r),
            _ => {
                set_err(err, "schema or relation is not valid UTF-8");
                return ptr::null_mut();
            }
        }
    };
    let written = runtime().block_on(async {
        let Some(dialect) = h.dialect else {
            return Err(dbconn::DbError::new(
                "this build does not write DDL for this database",
            ));
        };
        let listed = h.driver.relations(s).await?;
        match listed.into_iter().find(|info| info.name == r) {
            Some(info) => dbddl::definition(h.driver.as_ref(), dialect, &info).await,
            None => Err(dbconn::DbError::new(format!("{s}.{r} is not there"))),
        }
    });
    match written.and_then(|text| {
        CString::new(text).map_err(|e| dbconn::DbError::new(format!("DDL is not text: {e}")))
    }) {
        Ok(text) => text.into_raw(),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// The statements a grid's pending changes would take, as a JSON array of
/// strings. Release with `db_string_free`.
///
/// Written and not run. What comes back goes to the server through `db_query`
/// like anything else, which is what puts it inside whatever transaction the
/// connection is in, under the same Cancel button and with the same error
/// positions — and what lets a front end show somebody the statements before
/// they run, which is the reason for editing through generated SQL at all.
///
/// `edits` is one relation's worth of changes:
///
/// ```json
/// {"schema": …, "relation": …,
///  "updates": [{"key": [{"column": …, "value": …}], "set": [{…}]}],
///  "inserts": [{"set": [{…}]}],
///  "deletes": [{"key": [{…}]}]}
/// ```
///
/// A `value` of JSON null is SQL's NULL; a value of `""` is an empty string. A
/// grid has to be able to say both, and one string cannot.
///
/// Refuses rather than guesses: a relation with no primary key has no way to
/// name one of its rows, a key that is not the whole key would name a set of
/// them, and text that is not a number never reaches a numeric column.
///
/// # Safety
/// `handle` must be live; `edits` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_edit_sql_json(
    handle: *mut DbHandle,
    edits: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || edits.is_null() {
        unsafe { set_err(err, "null handle or edits") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let text = match unsafe { CStr::from_ptr(edits) }.to_str() {
        Ok(text) => text,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    let requested: dbedit::Edits = match serde_json::from_str(text) {
        Ok(requested) => requested,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    // The connection's own dialect and not the editor's guess, for the reason
    // `DbHandle::dialect` gives: an UPDATE written in PostgreSQL's quoting for a
    // database that is not PostgreSQL is a statement somebody's data goes into.
    let Some(dialect) = h.dialect else {
        unsafe {
            set_err(
                err,
                "this build does not write statements for this database",
            )
        };
        return ptr::null_mut();
    };
    match runtime().block_on(dbedit::statements(h.driver.as_ref(), dialect, &requested)) {
        Ok(statements) => json_result(&statements, err),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// A stack of filter rows as one WHERE clause, as plain text. Release with
/// `db_string_free`.
///
/// ```json
/// {"schema": …, "relation": …,
///  "rules": [{"column": …, "op": …, "value": …, "second": …}]}
/// ```
///
/// `op` is one of the names `db_filter_columns_json` offered for that column.
/// `value` is the text as it was typed and never pre-quoted — the quoting is
/// this side's, and a caller that did it too is how a filter starts matching
/// literal apostrophes. `second` is the far end of a `between`, and absent for
/// every other operator.
///
/// The rules are ANDed in the order given. No rules answers an empty string,
/// which is the unfiltered browse rather than a failure. One rule is an
/// ordinary stack too: it is what the grid's cell menu sends, and there is no
/// second entry point for it.
///
/// Written here rather than in the front end for the reason `db_edit_sql_json`
/// is, with one more of its own. Quoting is the database's own and whether a
/// value goes in bare or in quotes depends on its column's declared type; and
/// `contains` is a `LIKE`, a `LIKE` needs an escape character, and which
/// character it may be is the database's own as well. A front end that guessed
/// at either would write filters that read a typed `%` as a wildcard.
///
/// # Safety
/// `handle` must be live; `filter` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_filter_clause(
    handle: *mut DbHandle,
    filter: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || filter.is_null() {
        unsafe { set_err(err, "null handle or filter") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let text = match unsafe { CStr::from_ptr(filter) }.to_str() {
        Ok(text) => text,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    let requested: dbedit::RowFilter = match serde_json::from_str(text) {
        Ok(requested) => requested,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    let Some(dialect) = h.dialect else {
        unsafe {
            set_err(
                err,
                "this build does not write statements for this database",
            )
        };
        return ptr::null_mut();
    };
    match runtime().block_on(dbedit::filter_clause(
        h.driver.as_ref(),
        dialect,
        &requested,
    )) {
        Ok(clause) => match CString::new(clause) {
            Ok(c) => c.into_raw(),
            Err(e) => {
                unsafe { set_err(err, e) };
                ptr::null_mut()
            }
        },
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Which columns a relation can be filtered on, and what each one may be asked,
/// as JSON. Release with `db_string_free`.
///
/// ```json
/// [{"name": "qty", "data_type": "numeric", "operators": ["equals", "between", …]}]
/// ```
///
/// The list is per column and per database. A column of a type nothing can be
/// ordered by is not offered `less_than`; a text column is offered `contains`
/// only where this database takes an `ESCAPE` clause.
///
/// Asked for rather than worked out from `db_columns_json`, for the reason
/// `db_row_identity_json` is asked for: a popup built from a second copy of that
/// rule would offer an operator `db_filter_clause` then refuses to write, and
/// the refusal would arrive as an error over a filter somebody had already
/// typed.
///
/// # Safety
/// `handle` must be live; `schema` and `relation` must be valid NUL-terminated C
/// strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_filter_columns_json(
    handle: *mut DbHandle,
    schema: *const c_char,
    relation: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || schema.is_null() || relation.is_null() {
        unsafe { set_err(err, "null handle, schema, or relation") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let (s, r) = unsafe {
        match (
            CStr::from_ptr(schema).to_str(),
            CStr::from_ptr(relation).to_str(),
        ) {
            (Ok(s), Ok(r)) => (s, r),
            _ => {
                set_err(err, "schema or relation is not valid UTF-8");
                return ptr::null_mut();
            }
        }
    };
    // The dialect is needed here and not only in `db_filter_clause` because it
    // decides what may be offered, not just what may be written: the answer to
    // "can this database escape a LIKE" is what keeps `contains` off a popup for
    // the databases that cannot.
    let Some(dialect) = h.dialect else {
        unsafe {
            set_err(
                err,
                "this build does not write statements for this database",
            )
        };
        return ptr::null_mut();
    };
    match runtime().block_on(dbedit::filter_columns(h.driver.as_ref(), dialect, s, r)) {
        Ok(columns) => json_result(&columns, err),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// What names one row of a relation, as JSON. Release with `db_string_free`.
///
/// ```json
/// {"columns": ["id"], "obstacle": null}
/// {"columns": [], "obstacle": "app.audit has no primary key or unique key, …"}
/// ```
///
/// The primary key where there is one, otherwise the narrowest `UNIQUE`
/// constraint whose columns are all NOT NULL. A front end asks rather than works
/// it out, for the reason `db_browse_statement` exists: the last thing this side
/// had that both ends computed separately was the browse statement, and the two
/// answers differed on every database but the one they were written against.
///
/// An empty `columns` is not a failure and does not set `err`. It is the ordinary
/// answer for a table nothing can name a row of, and `obstacle` is the sentence
/// to put on screen — naming the table, and naming the constraint that had to be
/// turned down where there was one. `err` is set only when the catalog could not
/// be read at all.
///
/// # Safety
/// `handle` must be live; `schema` and `relation` must be valid NUL-terminated C
/// strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_row_identity_json(
    handle: *mut DbHandle,
    schema: *const c_char,
    relation: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() || schema.is_null() || relation.is_null() {
        unsafe { set_err(err, "null handle, schema, or relation") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let (s, r) = unsafe {
        match (
            CStr::from_ptr(schema).to_str(),
            CStr::from_ptr(relation).to_str(),
        ) {
            (Ok(s), Ok(r)) => (s, r),
            _ => {
                set_err(err, "schema or relation is not valid UTF-8");
                return ptr::null_mut();
            }
        }
    };
    match runtime().block_on(dbedit::identity(h.driver.as_ref(), s, r)) {
        Ok(identity) => json_result(&identity, err),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

/// What this connection's transaction is doing, as JSON: whether control is
/// possible at all, whether it is in autocommit, whether one is open, and which
/// savepoints are set. Release with `db_string_free`.
///
/// Pulled after the calls that could have changed it rather than pushed. The
/// front end already redraws at exactly those moments, and a second way of
/// finding out would be a second thing to keep in step with the first.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_tx_state_json(
    handle: *mut DbHandle,
    err: *mut *mut c_char,
) -> *mut c_char {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    json_result(&h.session.state(h.driver.as_ref()), err)
}

/// Turns autocommit on (non-zero) or off (0). Returns 0 on success, -1 on
/// failure.
///
/// Sends nothing by itself: the mode decides what happens to the *next*
/// statement, and a connection with nothing open has nothing to tell the server
/// yet. Refused while a transaction is open, and refused on a connection that
/// cannot hold one.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_tx_autocommit(
    handle: *mut DbHandle,
    on: c_int,
    err: *mut *mut c_char,
) -> c_int {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return -1;
    }
    let h = unsafe { &*handle };
    match h.session.set_autocommit(h.driver.as_ref(), on != 0) {
        Ok(()) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Ends the open transaction and keeps what it did. Returns 0 on success, -1 on
/// failure — including when there was nothing open, which is a front end and a
/// connection disagreeing about the state rather than a harmless no-op.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_tx_commit(handle: *mut DbHandle, err: *mut *mut c_char) -> c_int {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return -1;
    }
    let h = unsafe { &*handle };
    match runtime().block_on(h.session.commit(h.driver.as_ref())) {
        Ok(()) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Ends the open transaction and undoes it. Returns 0 on success, -1 on failure.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_tx_rollback(handle: *mut DbHandle, err: *mut *mut c_char) -> c_int {
    if handle.is_null() {
        unsafe { set_err(err, "null handle") };
        return -1;
    }
    let h = unsafe { &*handle };
    match runtime().block_on(h.session.rollback(h.driver.as_ref())) {
        Ok(()) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Defines one `(handle, name, err) -> int` savepoint entry point.
///
/// The three differ only in which step they take, and written out they would be
/// three copies of the same null check and the same UTF-8 handling.
macro_rules! savepoint_step {
    ($(#[$doc:meta])* $name:ident => $method:ident) => {
        $(#[$doc])*
        ///
        /// Returns 0 on success and -1 on failure. A name is a letter followed
        /// by letters, digits or underscores; anything else is refused, because
        /// a savepoint name reaches the server as an identifier written into the
        /// statement and there is no placeholder to bind it to.
        ///
        /// # Safety
        /// `handle` must be live; `name` must be a valid NUL-terminated C string.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            handle: *mut DbHandle,
            name: *const c_char,
            err: *mut *mut c_char,
        ) -> c_int {
            if handle.is_null() || name.is_null() {
                unsafe { set_err(err, "null handle or name") };
                return -1;
            }
            let h = unsafe { &*handle };
            let n = match unsafe { CStr::from_ptr(name) }.to_str() {
                Ok(n) => n,
                Err(e) => {
                    unsafe { set_err(err, e) };
                    return -1;
                }
            };
            match runtime().block_on(h.session.$method(h.driver.as_ref(), n)) {
                Ok(()) => 0,
                Err(e) => {
                    unsafe { set_err(err, e) };
                    -1
                }
            }
        }
    };
}

savepoint_step! {
    /// Marks a point in the open transaction to come back to.
    db_tx_savepoint => savepoint
}

savepoint_step! {
    /// Undoes what the transaction did after `name` was marked, leaving the
    /// transaction open.
    db_tx_rollback_to => rollback_to
}

savepoint_step! {
    /// Forgets `name` and the savepoints inside it, keeping the work they marked.
    db_tx_release => release
}

/// Prepares `statement` and returns a stream over its results.
///
/// `err_position` carries the server's error cursor as a number rather than
/// leaving it inside the message: it is the one part of an error the front end
/// acts on rather than displays, and recovering it by re-reading the prose is
/// how a caret ends up pointing at whatever the sentence happened to contain.
/// Zero means the error has no position, which is unambiguous because the
/// server counts from one.
///
/// # Safety
/// `handle` must be live; `statement` must be a valid NUL-terminated C string;
/// `err_position` must be null or point to writable `int` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query(
    handle: *mut DbHandle,
    statement: *const c_char,
    batch_rows: usize,
    err: *mut *mut c_char,
    err_position: *mut c_int,
) -> *mut DbQuery {
    if !err_position.is_null() {
        unsafe { *err_position = 0 };
    }
    if handle.is_null() || statement.is_null() {
        unsafe { set_err(err, "null handle or statement") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let s = match unsafe { CStr::from_ptr(statement) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    // Where a transaction opens in manual-commit mode, so that the `BEGIN` and
    // the statement it belongs to are one call from the front end's point of
    // view — two would be two chances to get the order wrong, on the one path
    // where the order is the whole point.
    if let Err(e) = runtime().block_on(h.session.before_statement(h.driver.as_ref())) {
        unsafe { set_err(err, e) };
        return ptr::null_mut();
    }
    match runtime().block_on(h.driver.query(s, batch_rows)) {
        Ok(stream) => Box::into_raw(Box::new(DbQuery { stream })),
        Err(e) => {
            if let (false, Some(p)) = (err_position.is_null(), e.statement_position()) {
                unsafe { *err_position = c_int::try_from(p).unwrap_or(0) };
            }
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Exports the result schema into `out` (an `ArrowSchema` the caller owns and
/// releases through its own `release` callback). Returns 0 on success.
///
/// # Safety
/// `query` must be live; `out` must point to writable `ArrowSchema` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query_schema(
    query: *mut DbQuery,
    out: *mut FFI_ArrowSchema,
    err: *mut *mut c_char,
) -> c_int {
    if query.is_null() || out.is_null() {
        unsafe { set_err(err, "null query or out") };
        return -1;
    }
    let q = unsafe { &*query };
    // The batch is exported as a struct array, so the schema it must match is
    // the struct of its fields rather than the bare field list.
    let dt = arrow::datatypes::DataType::Struct(q.stream.schema().fields().clone());
    match FFI_ArrowSchema::try_from(&dt) {
        Ok(schema) => {
            unsafe { ptr::write(out, schema) };
            0
        }
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Pulls the next batch. Returns 1 when `out` was filled, 0 when the result is
/// exhausted, -1 on error, -2 when the statement was cancelled.
///
/// Cancellation gets a code of its own because it is not a fault and should not
/// be reported as one. `err` is still set, so a caller that only distinguishes
/// success from failure keeps working and merely says "canceling statement due
/// to user request" where it could have said "Cancelled".
///
/// # Safety
/// `query` must be live; `out` must point to writable `ArrowArray` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query_next(
    query: *mut DbQuery,
    out: *mut FFI_ArrowArray,
    err: *mut *mut c_char,
) -> c_int {
    if query.is_null() || out.is_null() {
        unsafe { set_err(err, "null query or out") };
        return -1;
    }
    let q = unsafe { &mut *query };
    match runtime().block_on(q.stream.next_batch()) {
        Ok(Some(batch)) => {
            let array = batch_to_ffi(batch);
            unsafe { ptr::write(out, array) };
            1
        }
        Ok(None) => 0,
        Err(e) => {
            let cancelled = e.is_cancelled();
            unsafe { set_err(err, e) };
            if cancelled { -2 } else { -1 }
        }
    }
}

/// Rows the statement reported affecting, or -1 while the result has not been
/// read to the end.
///
/// The only thing a statement returning no rows says about itself. Negative for
/// "not known yet" rather than zero, because zero is a real answer — an UPDATE
/// that matched nothing — and a front end that cannot tell those apart reports a
/// statement as having done nothing when it has not finished doing it.
///
/// # Safety
/// `query` must come from `db_query` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query_rows_affected(query: *mut DbQuery) -> i64 {
    if query.is_null() {
        return -1;
    }
    let q = unsafe { &*query };
    q.stream
        .rows_affected()
        .map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX))
}

fn batch_to_ffi(batch: RecordBatch) -> FFI_ArrowArray {
    let struct_array: StructArray = batch.into();
    FFI_ArrowArray::new(&struct_array.to_data())
}

/// # Safety
/// `query` must come from `db_query` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query_free(query: *mut DbQuery) {
    if !query.is_null() {
        drop(unsafe { Box::from_raw(query) });
    }
}

/// # Safety
/// `s` must be a string produced by this library's `err` out-parameters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Opens a cursor over `statement` and returns a handle to fetch pages.
///
/// A cursor occupies its connection while open, so the handle owns a
/// connection of its own for the lifetime of the cursor. Freeing the cursor
/// closes that connection, which is what rolls its transaction back — so a
/// front-end that drops a result mid-scroll leaves nothing behind.
///
/// # Safety
/// `handle` must be live; `statement` must be a valid NUL-terminated C string;
/// `err_position` must be null or point to writable `int` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cursor(
    handle: *mut DbHandle,
    statement: *const c_char,
    batch_rows: usize,
    err: *mut *mut c_char,
    err_position: *mut c_int,
) -> *mut DbCursor {
    if !err_position.is_null() {
        unsafe { *err_position = 0 };
    }
    if handle.is_null() || statement.is_null() {
        unsafe { set_err(err, "null handle or statement") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let s = match unsafe { CStr::from_ptr(statement) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    match runtime().block_on(h.driver.cursor(s, batch_rows)) {
        Ok(cursor) => {
            let cancel = cursor.canceller();
            Box::into_raw(Box::new(DbCursor { cursor, cancel }))
        }
        Err(e) => {
            if let (false, Some(p)) = (err_position.is_null(), e.statement_position()) {
                unsafe { *err_position = c_int::try_from(p).unwrap_or(0) };
            }
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Exports the cursor's schema into `out` (an `ArrowSchema` the caller owns and
/// releases through its own `release` callback). Returns 0 on success.
///
/// # Safety
/// `cursor` must be live; `out` must point to writable `ArrowSchema` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cursor_schema(
    cursor: *mut DbCursor,
    out: *mut FFI_ArrowSchema,
    err: *mut *mut c_char,
) -> c_int {
    if cursor.is_null() || out.is_null() {
        unsafe { set_err(err, "null cursor or out") };
        return -1;
    }
    let c = unsafe { &(*cursor).cursor };
    // Exported as a struct array, like the query path, so the schema has to be
    // the struct of the fields rather than the bare field list.
    let dt = arrow::datatypes::DataType::Struct(c.schema().fields().clone());
    match FFI_ArrowSchema::try_from(&dt) {
        Ok(schema) => {
            unsafe { ptr::write(out, schema) };
            0
        }
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Fetches the next batch of rows from the cursor.
///
/// Returns 1 when `out` was filled, 0 when the result is exhausted,
/// -1 on error, -2 when the statement was cancelled.
///
/// # Safety
/// `cursor` must be live; `out` must point to writable `ArrowArray` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cursor_next(
    cursor: *mut DbCursor,
    out: *mut FFI_ArrowArray,
    err: *mut *mut c_char,
) -> c_int {
    if cursor.is_null() || out.is_null() {
        unsafe { set_err(err, "null cursor or out") };
        return -1;
    }
    let c = unsafe { &mut (*cursor).cursor };
    match runtime().block_on(c.fetch()) {
        Ok(Some(batch)) => {
            let array = batch_to_ffi(batch);
            unsafe { ptr::write(out, array) };
            1
        }
        Ok(None) => 0,
        Err(e) => {
            let cancelled = e.is_cancelled();
            unsafe { set_err(err, e) };
            if cancelled { -2 } else { -1 }
        }
    }
}

/// Asks the server to stop the fetch this cursor is running. Returns 0 when the
/// request was delivered, -1 when it could not be.
///
/// The one call here that may be made while another is in flight on the same
/// cursor, and it has to be: `db_cursor_next` blocks for as long as the server
/// takes, so a cancel that waited its turn would arrive after the page it exists
/// to interrupt. Sound because it borrows only the canceller, which the fetch
/// does not touch, and travels on a connection of its own.
///
/// `db_cancel` cannot do this job. That one cancels the session connection, and
/// a cursor runs on one of its own — a front-end whose browse pane reads through
/// a cursor has to route its Cancel here or the button does nothing.
///
/// Delivery is not interruption, as with `db_cancel`: the outcome is observable
/// only where the fetch is, as `db_cursor_next` answering -2.
///
/// # Safety
/// `cursor` must come from `db_cursor` and not have been freed. It must not be
/// freed concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cursor_cancel(cursor: *mut DbCursor, err: *mut *mut c_char) -> c_int {
    if cursor.is_null() {
        unsafe { set_err(err, "null cursor") };
        return -1;
    }
    let cancel = unsafe { &(*cursor).cancel };
    match runtime().block_on(cancel.cancel()) {
        Ok(()) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Closes the cursor explicitly.
///
/// This is optional as the cursor will be closed automatically when dropped.
///
/// # Safety
/// `cursor` must come from `db_cursor` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cursor_close(cursor: *mut DbCursor, err: *mut *mut c_char) -> c_int {
    if cursor.is_null() {
        unsafe { set_err(err, "null cursor") };
        return -1;
    }
    let c = unsafe { &mut (*cursor).cursor };
    match runtime().block_on(c.close()) {
        Ok(()) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// # Safety
/// `cursor` must come from `db_cursor` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cursor_free(cursor: *mut DbCursor) {
    if !cursor.is_null() {
        drop(unsafe { Box::from_raw(cursor) });
    }
}

/// How many rows an export may still write, from what the caller asked for.
///
/// Anything at or below zero is no limit: 0 is how a caller says "all of it"
/// across a boundary that has no `Option`, and a negative count is a caller
/// that computed one — refusing there would turn an arithmetic slip into a
/// failed export rather than a complete one.
fn row_limit_of(row_limit: i64) -> Option<u64> {
    (row_limit > 0).then_some(row_limit as u64)
}

/// Trims `batch` to what `remaining` still allows, or ends the export.
///
/// Returns `None` once the limit is spent, which is what stops the iterator —
/// and stops it without another round trip to the server, since the batch that
/// would have overshot is the last one fetched.
fn take_up_to(
    batch: arrow::array::RecordBatch,
    remaining: &mut Option<u64>,
) -> Option<arrow::array::RecordBatch> {
    let Some(left) = remaining else {
        return Some(batch);
    };
    if *left == 0 {
        return None;
    }
    let rows = batch.num_rows() as u64;
    if rows <= *left {
        *left -= rows;
        return Some(batch);
    }
    let wanted = *left as usize;
    *left = 0;
    Some(batch.slice(0, wanted))
}

/// Drains `cursor` into the file at `path`, written as `format` — one of the
/// extensions `Format::from_extension` knows.
///
/// Returns the number of rows written, -1 on error, -2 when the statement was
/// cancelled, which is the convention `db_cursor_next` uses.
///
/// Takes a cursor rather than a statement because the caller already has
/// `db_cursor_cancel` for it. A statement would need a cancel path of its own,
/// and cancelling an export is not a different problem from cancelling the
/// fetch that feeds it.
///
/// Nothing is held: batches are written and dropped as they arrive, so the
/// size of the result bounds the file and not the memory. This is the whole
/// reason the front end does not do this itself.
///
/// `row_limit` of 0 writes every row. A limit exists so that "only the rows
/// already on screen" can be offered without a second writer somewhere else
/// that has to be kept saying the same thing as this one — it is this path,
/// stopping early. The batch that crosses the limit is sliced rather than
/// dropped, because a limit that rounds down to the batch size is not the
/// number the caller was shown.
///
/// # Safety
/// `cursor` must come from `db_cursor` and not have been freed, and no other
/// call may be in flight on it. `format` and `path` must be valid
/// NUL-terminated C strings. `err` must be null or point to writable storage
/// for one `char *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_export(
    cursor: *mut DbCursor,
    format: *const c_char,
    path: *const c_char,
    row_limit: i64,
    err: *mut *mut c_char,
) -> i64 {
    if cursor.is_null() || format.is_null() || path.is_null() {
        unsafe { set_err(err, "null cursor, format, or path") };
        return -1;
    }
    let format_str = match unsafe { CStr::from_ptr(format) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let format = match dbtransfer::Format::from_extension(format_str) {
        Some(f) => f,
        None => {
            unsafe { set_err(err, format!("no exporter writes {format_str:?} files")) };
            return -1;
        }
    };
    // Created before a row is fetched, so an unwritable location is reported
    // instead of a query being run for a file that was never going to open.
    let file = match std::fs::File::create(path_str) {
        Ok(f) => f,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let writer = std::io::BufWriter::new(file);
    let c = unsafe { &mut (*cursor).cursor };
    // The driver's error is kept whole rather than folded into the ArrowError
    // that stops the writer: `ArrowError` has nowhere to put `cancelled`, and
    // the difference between "the server refused this" and "you pressed Stop"
    // is the difference between an error banner and none.
    let mut failure = None;
    let mut remaining = row_limit_of(row_limit);
    let rows = {
        // One `block_on` per batch, as `db_cursor_next` does — this call owns
        // the thread for the length of the export, and the front end runs it
        // off its own.
        let batches = std::iter::from_fn(|| match runtime().block_on(c.fetch()) {
            Ok(Some(batch)) => Some(Ok(take_up_to(batch, &mut remaining)?)),
            Ok(None) => None,
            Err(e) => {
                let message = e.to_string();
                failure = Some(e);
                Some(Err(arrow::error::ArrowError::ComputeError(message)))
            }
        });
        dbtransfer::export(batches, format, writer)
    };
    match rows {
        Ok(n) => i64::try_from(n).unwrap_or(i64::MAX),
        Err(e) => {
            // The partial file goes: it was truncated on create, so there is no
            // earlier version to preserve, and one that stops mid-result looks
            // exactly like a complete one to whoever opens it next.
            let _ = std::fs::remove_file(path_str);
            match failure {
                Some(f) => {
                    let cancelled = f.is_cancelled();
                    unsafe { set_err(err, f) };
                    if cancelled { -2 } else { -1 }
                }
                None => {
                    unsafe { set_err(err, e) };
                    -1
                }
            }
        }
    }
}

/// Drains `cursor` into the file at `path` as `INSERT` statements for `table`.
///
/// Its own entry point rather than a fifth `Format` for `db_export`, because
/// `INSERT` needs a table to name and a dialect to spell it in — four of the
/// other five formats would get two arguments that mean nothing to them.
///
/// Fails when this build has no dialect for the database, because a script in
/// the wrong spelling is not a lesser version of the right one — it is one that
/// runs somewhere it should not.
///
/// Everything else — the return convention, taking a cursor, holding nothing —
/// is as `db_export` documents it.
///
/// # Safety
/// `handle` must come from `db_connect` and not have been freed. `cursor` must
/// come from `db_cursor` and not have been freed, and no other call may be in
/// flight on it. `table` and `path` must be valid NUL-terminated C strings.
/// `err` must be null or point to writable storage for one `char *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_export_sql(
    handle: *mut DbHandle,
    cursor: *mut DbCursor,
    table: *const c_char,
    path: *const c_char,
    row_limit: i64,
    err: *mut *mut c_char,
) -> i64 {
    if handle.is_null() || cursor.is_null() || table.is_null() || path.is_null() {
        unsafe { set_err(err, "null handle, cursor, table, or path") };
        return -1;
    }
    let h = unsafe { &*handle };
    let table_str = match unsafe { CStr::from_ptr(table) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let Some(dialect) = h.dialect else {
        unsafe { set_err(err, "this build has no dialect for this database") };
        return -1;
    };
    // Created before a row is fetched, so an unwritable location is reported
    // instead of a query being run for a file that was never going to open.
    let file = match std::fs::File::create(path_str) {
        Ok(f) => f,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let writer = std::io::BufWriter::new(file);
    let c = unsafe { &mut (*cursor).cursor };
    // The driver's error is kept whole rather than folded into the ArrowError
    // that stops the writer: `ArrowError` has nowhere to put `cancelled`, and
    // the difference between "the server refused this" and "you pressed Stop"
    // is the difference between an error banner and none.
    let mut failure = None;
    let mut remaining = row_limit_of(row_limit);
    let rows = {
        // One `block_on` per batch, as `db_cursor_next` does — this call owns
        // the thread for the length of the export, and the front end runs it
        // off its own.
        let batches = std::iter::from_fn(|| match runtime().block_on(c.fetch()) {
            Ok(Some(batch)) => Some(Ok(take_up_to(batch, &mut remaining)?)),
            Ok(None) => None,
            Err(e) => {
                let message = e.to_string();
                failure = Some(e);
                Some(Err(arrow::error::ArrowError::ComputeError(message)))
            }
        });
        dbtransfer::export_sql(batches, dialect, table_str.to_string(), writer)
    };
    match rows {
        Ok(n) => i64::try_from(n).unwrap_or(i64::MAX),
        Err(e) => {
            // The partial file goes: it was truncated on create, so there is no
            // earlier version to preserve, and one that stops mid-result looks
            // exactly like a complete one to whoever opens it next.
            let _ = std::fs::remove_file(path_str);
            match failure {
                Some(f) => {
                    let cancelled = f.is_cancelled();
                    unsafe { set_err(err, f) };
                    if cancelled { -2 } else { -1 }
                }
                None => {
                    unsafe { set_err(err, e) };
                    -1
                }
            }
        }
    }
}

/// Drains the cursor into `target` as INSERT statements for `table`.
///
/// The dialect comes from the target connection, because the statements are
/// written for the database they are being sent to — a DuckDB cursor feeding
/// a PostgreSQL target needs PostgreSQL quoting, and the source's dialect is
/// irrelevant to the INSERTs that reach the server.
///
/// Returns the row count on success. Returns -1 when the target refused the
/// statement (a table that does not exist, a type mismatch, a constraint
/// violation) and -2 when the source was cancelled — the same convention
/// `db_export_sql` uses, so a caller that already handles one can handle the
/// other without a second branch.
///
/// # Safety
/// `cursor` must come from `db_cursor` and not have been freed. `target` must
/// come from `db_connect` and not have been freed. `table` must be a valid
/// NUL-terminated C string. `err` must be null or point to writable storage
/// for one `char *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_transfer(
    cursor: *mut DbCursor,
    target: *mut DbHandle,
    table: *const c_char,
    err: *mut *mut c_char,
) -> i64 {
    if cursor.is_null() || target.is_null() || table.is_null() {
        unsafe { set_err(err, "null cursor, target, or table") };
        return -1;
    }
    let table_str = match unsafe { CStr::from_ptr(table) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let t = unsafe { &*target };
    let Some(dialect) = t.dialect else {
        unsafe { set_err(err, "this build has no dialect for this database") };
        return -1;
    };
    let c = unsafe { &mut (*cursor).cursor };
    match runtime().block_on(dbtransfer::transfer(
        c.as_mut(),
        t.driver.as_ref(),
        dialect,
        table_str.to_string(),
    )) {
        Ok(n) => i64::try_from(n).unwrap_or(i64::MAX),
        Err(e) => {
            let cancelled = e.is_cancelled();
            unsafe { set_err(err, e) };
            if cancelled { -2 } else { -1 }
        }
    }
}

/// Reads a file into an existing table on `target`.
///
/// The file is read in batches so a multi-gigabyte CSV is no heavier than a
/// small one — the same property `transfer` has, and the reason both live in
/// this crate rather than in the caller. The table must already exist: this
/// does not guess a schema, because a file's types are only meaningful in the
/// context of the table they are being read into.
///
/// Returns the row count on success. Returns -1 when the target refused the
/// statement (a table that does not exist, a type mismatch, a constraint
/// violation) and -2 when the operation was cancelled — the same convention
/// `db_transfer` uses, so a caller that already handles one can handle the
/// other without a second branch.
///
/// # Safety
/// `target` must come from `db_connect` and not have been freed. `format`,
/// `path`, and `table` must be valid NUL-terminated C strings. `err` must be
/// null or point to writable storage for one `char *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_import(
    target: *mut DbHandle,
    format: *const c_char,
    path: *const c_char,
    table: *const c_char,
    err: *mut *mut c_char,
) -> i64 {
    if target.is_null() || format.is_null() || path.is_null() || table.is_null() {
        unsafe { set_err(err, "null target, format, path, or table") };
        return -1;
    }
    let format_str = match unsafe { CStr::from_ptr(format) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let table_str = match unsafe { CStr::from_ptr(table) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return -1;
        }
    };
    let format = match dbtransfer::Format::from_extension(format_str) {
        Some(f) => f,
        None => {
            unsafe { set_err(err, format!("no importer reads {format_str:?} files")) };
            return -1;
        }
    };
    let t = unsafe { &*target };
    let Some(dialect) = t.dialect else {
        unsafe { set_err(err, "this build has no dialect for this database") };
        return -1;
    };
    match runtime().block_on(dbtransfer::import(
        std::path::Path::new(path_str),
        format,
        t.driver.as_ref(),
        dialect,
        table_str.to_string(),
    )) {
        Ok(n) => i64::try_from(n).unwrap_or(i64::MAX),
        Err(e) => {
            let cancelled = e.is_cancelled();
            unsafe { set_err(err, e) };
            if cancelled { -2 } else { -1 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where `make db-up-ssh` wrote the fixture's host keys.
    ///
    /// From the environment first, with a compile-time fallback, for the reason
    /// the tunnel crate's own copy gives: one `target/` shared between git
    /// worktrees hands back a test binary naming whichever worktree built it.
    fn known_hosts() -> CString {
        let path = std::env::var("SSH_KNOWN_HOSTS").unwrap_or_else(|_| {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/ssh/known_hosts").to_owned()
        });
        CString::new(path).expect("a path with no NUL in it")
    }

    /// Whatever the core wrote into `err`, freed the way a caller would.
    fn taken(err: &mut *mut c_char) -> String {
        if err.is_null() {
            return "no message".to_owned();
        }
        let message = unsafe { CStr::from_ptr(*err) }
            .to_string_lossy()
            .into_owned();
        unsafe { db_string_free(*err) };
        *err = ptr::null_mut();
        message
    }

    /// A connection made the way the application makes it, through a bastion,
    /// and then asked a question.
    ///
    /// The question is the test, and it is the schemas rather than a ping.
    /// Opening through a tunnel and stopping there would pass against a build
    /// that dropped the forward on the way out of `db_connect` — the driver has
    /// already dialled by then, and closing the local listener does not disturb
    /// the socket it dialled through. What a dropped forward actually kills is
    /// every connection opened *after* it, and this driver's pool starts empty:
    /// the session answers `SELECT version()` from the socket it already has,
    /// and the first metadata call is the one that has to dial. Measured both
    /// ways — `db_ping` still answers through a forward that is gone.
    #[test]
    #[ignore = "requires the SSH server and the benchmark database (make db-up-ssh db-up)"]
    fn a_connection_opened_through_a_bastion_still_answers() {
        // `pg` resolves on the compose network and nowhere else, so a forward
        // ending anywhere but the far side of the bastion could not reach it.
        // No port written down, so the driver's own default is what is
        // forwarded to.
        let conn = CString::new("postgres://bench:bench@pg/bench").unwrap();
        let host = CString::new("127.0.0.1").unwrap();
        let user = CString::new("bench").unwrap();
        let password = CString::new("bench").unwrap();
        let hosts = known_hosts();
        let ssh = DbSshConfig {
            host: host.as_ptr(),
            port: 52222,
            user: user.as_ptr(),
            password: password.as_ptr(),
            key_path: ptr::null(),
            passphrase: ptr::null(),
            known_hosts: hosts.as_ptr(),
        };
        let mut err: *mut c_char = ptr::null_mut();
        let handle = unsafe { db_connect(conn.as_ptr(), &ssh, 30, &mut err) };
        assert!(
            !handle.is_null(),
            "the connection did not open: {}",
            taken(&mut err)
        );
        let schemas = unsafe { db_schemas_json(handle, &mut err) };
        assert!(
            !schemas.is_null(),
            "the connection stopped answering after it was opened: {}",
            taken(&mut err)
        );
        let listed = unsafe { CStr::from_ptr(schemas) }
            .to_string_lossy()
            .into_owned();
        unsafe { db_string_free(schemas) };
        assert!(
            listed.contains("public"),
            "expected the benchmark database's schemas, got {listed}"
        );
        unsafe { db_free(handle) };
    }

    /// A bastion filled in wrong is refused before anything is dialled, and the
    /// message says which part.
    ///
    /// Not ignored, because none of these reaches a network: a struct that
    /// cannot be read is a fault this build answers on its own, and a check that
    /// needed a container to prove it is one nobody would run.
    #[test]
    fn a_bastion_filled_in_wrong_says_which_part() {
        let conn = CString::new("postgres://bench:bench@pg/bench").unwrap();
        let host = CString::new("127.0.0.1").unwrap();
        let user = CString::new("bench").unwrap();
        let password = CString::new("bench").unwrap();
        let key = CString::new("/dev/null").unwrap();
        let hosts = known_hosts();
        let config =
            |host: *const c_char, port: u16, password: *const c_char, key: *const c_char| {
                DbSshConfig {
                    host,
                    port,
                    user: user.as_ptr(),
                    password,
                    key_path: key,
                    passphrase: ptr::null(),
                    known_hosts: hosts.as_ptr(),
                }
            };

        let cases = [
            (
                "no host",
                config(ptr::null(), 52222, password.as_ptr(), ptr::null()),
                "host",
            ),
            (
                "no port",
                config(host.as_ptr(), 0, password.as_ptr(), ptr::null()),
                "port",
            ),
            (
                "a password and a key",
                config(host.as_ptr(), 52222, password.as_ptr(), key.as_ptr()),
                "not both",
            ),
            (
                "neither",
                config(host.as_ptr(), 52222, ptr::null(), ptr::null()),
                "password or a key file",
            ),
        ];

        for (label, ssh, expected) in cases {
            let mut err: *mut c_char = ptr::null_mut();
            let handle = unsafe { db_connect(conn.as_ptr(), &ssh, 30, &mut err) };
            assert!(
                handle.is_null(),
                "{label}: a bastion filled in wrong must not open a connection"
            );
            let message = taken(&mut err);
            assert!(
                message.contains(expected),
                "{label}: expected a message about {expected}, got {message}"
            );
        }
    }
}
