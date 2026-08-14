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
use dbsql::{Origin, TokenKind};
use session::Session;
use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("failed to start tokio runtime"))
}

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

/// Opens the database `conn_str` names.
///
/// The string starts with the driver it wants — `postgres://…`, `sqlite://…` —
/// and there is no fallback for one that does not. A bare `host=… port=…` is a
/// PostgreSQL string today and a MySQL string in the same shape tomorrow, and a
/// client that guesses between them is one that connects to the wrong database
/// without saying so.
///
/// # Safety
/// `conn_str` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_connect(
    conn_str: *const c_char,
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
    match runtime().block_on(registry::connect(s)) {
        Ok(driver) => {
            let driver: Arc<dyn Driver> = Arc::from(driver);
            let names = Names::new(driver.clone(), dbsql::for_scheme(registry::scheme_of(s)));
            Box::into_raw(Box::new(DbHandle {
                driver,
                names,
                session: Session::new(),
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
    let suggestions = runtime().block_on(h.names.suggest(&question));
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
        let listed = h.driver.relations(s).await?;
        match listed.into_iter().find(|info| info.name == r) {
            Some(info) => dbddl::definition(h.driver.as_ref(), h.names.dialect(), &info).await,
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

/// Prepares `sql` and returns a stream over its results.
///
/// `err_position` carries the server's error cursor as a number rather than
/// leaving it inside the message: it is the one part of an error the front end
/// acts on rather than displays, and recovering it by re-reading the prose is
/// how a caret ends up pointing at whatever the sentence happened to contain.
/// Zero means the error has no position, which is unambiguous because the
/// server counts from one.
///
/// # Safety
/// `handle` must be live; `sql` must be a valid NUL-terminated C string;
/// `err_position` must be null or point to writable `int` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query(
    handle: *mut DbHandle,
    sql: *const c_char,
    batch_rows: usize,
    err: *mut *mut c_char,
    err_position: *mut c_int,
) -> *mut DbQuery {
    if !err_position.is_null() {
        unsafe { *err_position = 0 };
    }
    if handle.is_null() || sql.is_null() {
        unsafe { set_err(err, "null handle or sql") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let s = match unsafe { CStr::from_ptr(sql) }.to_str() {
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

/// Opens a cursor over `sql` and returns a handle to fetch pages.
///
/// A cursor occupies its connection while open, so the handle owns a
/// connection of its own for the lifetime of the cursor. Freeing the cursor
/// closes that connection, which is what rolls its transaction back — so a
/// front-end that drops a result mid-scroll leaves nothing behind.
///
/// # Safety
/// `handle` must be live; `sql` must be a valid NUL-terminated C string;
/// `err_position` must be null or point to writable `int` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_cursor(
    handle: *mut DbHandle,
    sql: *const c_char,
    batch_rows: usize,
    err: *mut *mut c_char,
    err_position: *mut c_int,
) -> *mut DbCursor {
    if !err_position.is_null() {
        unsafe { *err_position = 0 };
    }
    if handle.is_null() || sql.is_null() {
        unsafe { set_err(err, "null handle or sql") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let s = match unsafe { CStr::from_ptr(sql) }.to_str() {
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
