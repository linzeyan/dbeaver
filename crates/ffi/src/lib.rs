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

use arrow::array::{Array, RecordBatch, StructArray};
use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use driver_postgres::{ArrowStream, PgSource};
use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;
use std::sync::OnceLock;
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

pub struct DbHandle {
    source: PgSource,
}

pub struct DbQuery {
    stream: ArrowStream,
}

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
    match runtime().block_on(PgSource::connect(s)) {
        Ok(source) => Box::into_raw(Box::new(DbHandle { source })),
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
/// call owns — it reads the handle to learn which backend to name, and that is
/// shared, immutable state.
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
    match runtime().block_on(h.source.cancel()) {
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
    match runtime().block_on(h.source.schemas()) {
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
    match runtime().block_on(h.source.relations(s)) {
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
            match runtime().block_on(h.source.$method(s, r)) {
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
    match runtime().block_on(h.source.query(s, batch_rows)) {
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
